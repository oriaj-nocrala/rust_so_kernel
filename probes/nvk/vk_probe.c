/*
 * A Vulkan program for NVK on constanos (docs/gpu/g4-nvkmd-plan.md): it goes through the driver's ICD entry point directly, no loader.
 * Built as one static musl executable with the whole of NVK linked in (probes/nvk/build.py).
 *
 *   - instance, physical device (properties, queues, memory types), logical device and queue
 *   - host-visible memory (system BO, mapped) and device-local memory (VRAM BO, not mappable)
 *   - a compute pipeline from SPIR-V: NAK compiles it here
 *   - a command buffer with a dispatch, a fence, binary and timeline semaphores
 *
 * On the software device nothing executes, so the buffer keeps its contents: the probe reports that, and fails only if the driver
 * returned an error. On the hardware device (G4c) the buffer must hold i * 3 + 1234 ("EXECUTED").
 */
#define VK_NO_PROTOTYPES
#include <vulkan/vulkan.h>

#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>

#include <signal.h>
#include <unistd.h>
#include <unwind.h>

#include "probe_spv.h"

/* An assertion in the driver ends in abort(): print where, as return addresses for addr2line (build.py leaves the unstripped
 * executable next to the stripped one). */
static _Unwind_Reason_Code trace_cb(struct _Unwind_Context *c, void *arg) {
   (void)arg;
   printf("VK BT %#lx\n", (unsigned long)_Unwind_GetIP(c));
   return _URC_NO_REASON;
}

/* Our own __assert_fail() and abort(): musl's frames carry no unwind information, so a trace that starts inside them stops at once.
 * Starting here, the first caller is the driver's own code. */
void __assert_fail(const char *expr, const char *file, int line, const char *func) {
   printf("VK ASSERT %s (%s: %s: %d)\n", expr, file, func, line);
   _Unwind_Backtrace(trace_cb, NULL);
   _exit(134);
}

void abort(void) {
   _Unwind_Backtrace(trace_cb, NULL);
   _exit(134);
}

extern PFN_vkVoidFunction vk_icdGetInstanceProcAddr(VkInstance instance, const char *name);

static int failures;
#define CHECK(cond, ...) do { if (cond) printf("VK ok   %s\n", #cond); else { failures++; printf("VK FAIL %s (line %d): ", #cond, __LINE__); printf(__VA_ARGS__); printf("\n"); } } while (0)
#define VKOK(call) do { VkResult r_ = (call); if (r_ != VK_SUCCESS) { failures++; printf("VK FAIL %s -> %d (line %d)\n", #call, (int)r_, __LINE__); goto done; } else printf("VK ok   %s\n", #call); } while (0)

#define GLOBAL(name) PFN_##name name = (PFN_##name)vk_icdGetInstanceProcAddr(NULL, #name)
#define INST(name) PFN_##name name = (PFN_##name)vk_icdGetInstanceProcAddr(instance, #name)
#define DEV(name) PFN_##name name = (PFN_##name)vkGetDeviceProcAddr(device, #name)

static uint64_t now_ns(void) {
   struct timespec t;
   clock_gettime(CLOCK_MONOTONIC, &t);
   return (uint64_t)t.tv_sec * 1000000000ull + t.tv_nsec;
}

static int find_type(const VkPhysicalDeviceMemoryProperties *mp, uint32_t allowed, VkMemoryPropertyFlags want, VkMemoryPropertyFlags avoid) {
   for (uint32_t i = 0; i < mp->memoryTypeCount; i++)
      if ((allowed & (1u << i)) && (mp->memoryTypes[i].propertyFlags & want) == want && !(mp->memoryTypes[i].propertyFlags & avoid))
         return (int)i;
   return -1;
}

int main(void) {
   setvbuf(stdout, NULL, _IONBF, 0);
   VkInstance instance = VK_NULL_HANDLE;
   VkDevice device = VK_NULL_HANDLE;
   VkBuffer buf = VK_NULL_HANDLE, vbuf = VK_NULL_HANDLE;
   VkDeviceMemory mem = VK_NULL_HANDLE, vmem = VK_NULL_HANDLE;
   VkShaderModule shader = VK_NULL_HANDLE;
   VkDescriptorSetLayout dsl = VK_NULL_HANDLE;
   VkPipelineLayout pl = VK_NULL_HANDLE;
   VkPipeline pipe = VK_NULL_HANDLE;
   VkDescriptorPool dpool = VK_NULL_HANDLE;
   VkCommandPool cpool = VK_NULL_HANDLE;
   VkFence fence = VK_NULL_HANDLE;
   VkSemaphore tsem = VK_NULL_HANDLE, bsem = VK_NULL_HANDLE;
   PFN_vkGetDeviceProcAddr vkGetDeviceProcAddr = NULL;

   GLOBAL(vkCreateInstance);
   CHECK(vkCreateInstance != NULL, "the ICD exports vkCreateInstance");
   if (!vkCreateInstance) return 1;

   VkApplicationInfo app = { .sType = VK_STRUCTURE_TYPE_APPLICATION_INFO, .pApplicationName = "vk_probe", .apiVersion = VK_API_VERSION_1_3 };
   VkInstanceCreateInfo ici = { .sType = VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO, .pApplicationInfo = &app };
   VKOK(vkCreateInstance(&ici, NULL, &instance));

   INST(vkDestroyInstance);
   INST(vkEnumeratePhysicalDevices);
   INST(vkGetPhysicalDeviceProperties);
   INST(vkGetPhysicalDeviceQueueFamilyProperties);
   INST(vkGetPhysicalDeviceMemoryProperties);
   INST(vkCreateDevice);
   vkGetDeviceProcAddr = (PFN_vkGetDeviceProcAddr)vk_icdGetInstanceProcAddr(instance, "vkGetDeviceProcAddr");

   uint32_t n = 0;
   VKOK(vkEnumeratePhysicalDevices(instance, &n, NULL));
   CHECK(n == 1, "one physical device (%u)", n);
   if (n == 0) {
      printf("VK no physical device: is /dev/nvgpu there and free?\n");
      goto done;
   }
   VkPhysicalDevice pdev = VK_NULL_HANDLE;
   n = 1;
   VkResult r = vkEnumeratePhysicalDevices(instance, &n, &pdev);
   CHECK(r == VK_SUCCESS && pdev != VK_NULL_HANDLE, "got it (%d)", (int)r);

   VkPhysicalDeviceProperties props;
   vkGetPhysicalDeviceProperties(pdev, &props);
   printf("VK device \"%s\" vendor %#x id %#x type %d api %u.%u.%u\n", props.deviceName, props.vendorID, props.deviceID, (int)props.deviceType,
          VK_VERSION_MAJOR(props.apiVersion), VK_VERSION_MINOR(props.apiVersion), VK_VERSION_PATCH(props.apiVersion));
   CHECK(props.vendorID == 0x10de && props.deviceType == VK_PHYSICAL_DEVICE_TYPE_DISCRETE_GPU, "an NVIDIA discrete GPU");

   uint32_t nq = 0;
   vkGetPhysicalDeviceQueueFamilyProperties(pdev, &nq, NULL);
   VkQueueFamilyProperties qf[16];
   if (nq > 16) nq = 16;
   vkGetPhysicalDeviceQueueFamilyProperties(pdev, &nq, qf);
   int family = -1;
   for (uint32_t i = 0; i < nq; i++) {
      printf("VK queue family %u: flags %#x count %u\n", i, qf[i].queueFlags, qf[i].queueCount);
      // a compute-only family first: its contexts ask for compute (+ transfer) engines, which the hardware device serves (graphics engines
      // are not wired up yet); fall back to the first family with compute
      if ((qf[i].queueFlags & VK_QUEUE_COMPUTE_BIT) && !(qf[i].queueFlags & VK_QUEUE_GRAPHICS_BIT) && (family < 0 || (qf[family].queueFlags & VK_QUEUE_GRAPHICS_BIT))) family = (int)i;
      else if (family < 0 && (qf[i].queueFlags & VK_QUEUE_COMPUTE_BIT)) family = (int)i;
   }
   printf("VK using queue family %d\n", family);
   CHECK(family >= 0, "a compute queue family");

   VkPhysicalDeviceMemoryProperties mp;
   vkGetPhysicalDeviceMemoryProperties(pdev, &mp);
   for (uint32_t i = 0; i < mp.memoryHeapCount; i++)
      printf("VK heap %u: %llu MiB flags %#x\n", i, (unsigned long long)(mp.memoryHeaps[i].size >> 20), mp.memoryHeaps[i].flags);
   for (uint32_t i = 0; i < mp.memoryTypeCount; i++)
      printf("VK type %u: heap %u flags %#x\n", i, mp.memoryTypes[i].heapIndex, mp.memoryTypes[i].propertyFlags);

   float prio = 1.0f;
   VkDeviceQueueCreateInfo qci = { .sType = VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO, .queueFamilyIndex = family, .queueCount = 1, .pQueuePriorities = &prio };
   VkPhysicalDeviceVulkan12Features f12 = { .sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_VULKAN_1_2_FEATURES, .timelineSemaphore = VK_TRUE };
   VkDeviceCreateInfo dci = { .sType = VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO, .pNext = &f12, .queueCreateInfoCount = 1, .pQueueCreateInfos = &qci };
   VKOK(vkCreateDevice(pdev, &dci, NULL, &device));

   DEV(vkDestroyDevice); DEV(vkGetDeviceQueue); DEV(vkCreateBuffer); DEV(vkDestroyBuffer); DEV(vkGetBufferMemoryRequirements);
   DEV(vkAllocateMemory); DEV(vkFreeMemory); DEV(vkBindBufferMemory); DEV(vkMapMemory); DEV(vkUnmapMemory);
   DEV(vkCreateShaderModule); DEV(vkDestroyShaderModule); DEV(vkCreateDescriptorSetLayout); DEV(vkDestroyDescriptorSetLayout);
   DEV(vkCreatePipelineLayout); DEV(vkDestroyPipelineLayout); DEV(vkCreateComputePipelines); DEV(vkDestroyPipeline);
   DEV(vkCreateDescriptorPool); DEV(vkDestroyDescriptorPool); DEV(vkAllocateDescriptorSets); DEV(vkUpdateDescriptorSets);
   DEV(vkCreateCommandPool); DEV(vkDestroyCommandPool); DEV(vkAllocateCommandBuffers); DEV(vkBeginCommandBuffer);
   DEV(vkEndCommandBuffer); DEV(vkCmdBindPipeline); DEV(vkCmdBindDescriptorSets); DEV(vkCmdDispatch);
   DEV(vkCreateFence); DEV(vkDestroyFence); DEV(vkQueueSubmit); DEV(vkWaitForFences); DEV(vkQueueWaitIdle); DEV(vkDeviceWaitIdle);
   DEV(vkCreateSemaphore); DEV(vkDestroySemaphore); DEV(vkWaitSemaphores); DEV(vkGetSemaphoreCounterValue); DEV(vkSignalSemaphore);
   DEV(vkResetFences);

   VkQueue queue;
   vkGetDeviceQueue(device, family, 0, &queue);
   CHECK(queue != VK_NULL_HANDLE, "a queue");

   // ---- memory: a host-visible storage buffer (system BO) and a device-local one (VRAM BO)
   const uint32_t N = 64;
   VkBufferCreateInfo bci = { .sType = VK_STRUCTURE_TYPE_BUFFER_CREATE_INFO, .size = N * 4, .usage = VK_BUFFER_USAGE_STORAGE_BUFFER_BIT,
                              .sharingMode = VK_SHARING_MODE_EXCLUSIVE };
   VKOK(vkCreateBuffer(device, &bci, NULL, &buf));
   VkMemoryRequirements req;
   vkGetBufferMemoryRequirements(device, buf, &req);
   int t = find_type(&mp, req.memoryTypeBits, VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT | VK_MEMORY_PROPERTY_HOST_COHERENT_BIT, 0);
   CHECK(t >= 0, "a host-visible coherent memory type for the buffer");
   if (t < 0) goto done;
   VkMemoryAllocateInfo mai = { .sType = VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO, .allocationSize = req.size, .memoryTypeIndex = t };
   VKOK(vkAllocateMemory(device, &mai, NULL, &mem));
   VKOK(vkBindBufferMemory(device, buf, mem, 0));
   uint32_t *data = NULL;
   VKOK(vkMapMemory(device, mem, 0, VK_WHOLE_SIZE, 0, (void **)&data));
   for (uint32_t i = 0; i < N; i++) data[i] = 0xdeadbeefu;
   CHECK(data[0] == 0xdeadbeefu && data[N - 1] == 0xdeadbeefu, "the mapping is writable and readable");

   VKOK(vkCreateBuffer(device, &bci, NULL, &vbuf));
   vkGetBufferMemoryRequirements(device, vbuf, &req);
   int vt = find_type(&mp, req.memoryTypeBits, VK_MEMORY_PROPERTY_DEVICE_LOCAL_BIT, VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT);
   CHECK(vt >= 0, "a device-local, not host-visible memory type");
   if (vt >= 0) {
      mai.allocationSize = req.size;
      mai.memoryTypeIndex = vt;
      VKOK(vkAllocateMemory(device, &mai, NULL, &vmem));
      VKOK(vkBindBufferMemory(device, vbuf, vmem, 0));
   }

   // ---- pipeline: NAK compiles the shader here
   VkShaderModuleCreateInfo smci = { .sType = VK_STRUCTURE_TYPE_SHADER_MODULE_CREATE_INFO, .codeSize = probe_spv_len, .pCode = (const uint32_t *)probe_spv };
   VKOK(vkCreateShaderModule(device, &smci, NULL, &shader));
   VkDescriptorSetLayoutBinding b0 = { .binding = 0, .descriptorType = VK_DESCRIPTOR_TYPE_STORAGE_BUFFER, .descriptorCount = 1, .stageFlags = VK_SHADER_STAGE_COMPUTE_BIT };
   VkDescriptorSetLayoutCreateInfo dslci = { .sType = VK_STRUCTURE_TYPE_DESCRIPTOR_SET_LAYOUT_CREATE_INFO, .bindingCount = 1, .pBindings = &b0 };
   VKOK(vkCreateDescriptorSetLayout(device, &dslci, NULL, &dsl));
   VkPipelineLayoutCreateInfo plci = { .sType = VK_STRUCTURE_TYPE_PIPELINE_LAYOUT_CREATE_INFO, .setLayoutCount = 1, .pSetLayouts = &dsl };
   VKOK(vkCreatePipelineLayout(device, &plci, NULL, &pl));
   VkComputePipelineCreateInfo cpci = { .sType = VK_STRUCTURE_TYPE_COMPUTE_PIPELINE_CREATE_INFO, .layout = pl,
      .stage = { .sType = VK_STRUCTURE_TYPE_PIPELINE_SHADER_STAGE_CREATE_INFO, .stage = VK_SHADER_STAGE_COMPUTE_BIT, .module = shader, .pName = "main" } };
   uint64_t t0 = now_ns();
   VKOK(vkCreateComputePipelines(device, VK_NULL_HANDLE, 1, &cpci, NULL, &pipe));
   printf("VK pipeline compiled in %llu us\n", (unsigned long long)((now_ns() - t0) / 1000));

   VkDescriptorPoolSize ps = { .type = VK_DESCRIPTOR_TYPE_STORAGE_BUFFER, .descriptorCount = 1 };
   VkDescriptorPoolCreateInfo dpci = { .sType = VK_STRUCTURE_TYPE_DESCRIPTOR_POOL_CREATE_INFO, .maxSets = 1, .poolSizeCount = 1, .pPoolSizes = &ps };
   VKOK(vkCreateDescriptorPool(device, &dpci, NULL, &dpool));
   VkDescriptorSetAllocateInfo dsai = { .sType = VK_STRUCTURE_TYPE_DESCRIPTOR_SET_ALLOCATE_INFO, .descriptorPool = dpool, .descriptorSetCount = 1, .pSetLayouts = &dsl };
   VkDescriptorSet ds;
   VKOK(vkAllocateDescriptorSets(device, &dsai, &ds));
   VkDescriptorBufferInfo dbi = { .buffer = buf, .offset = 0, .range = VK_WHOLE_SIZE };
   VkWriteDescriptorSet wds = { .sType = VK_STRUCTURE_TYPE_WRITE_DESCRIPTOR_SET, .dstSet = ds, .dstBinding = 0, .descriptorCount = 1,
                                .descriptorType = VK_DESCRIPTOR_TYPE_STORAGE_BUFFER, .pBufferInfo = &dbi };
   vkUpdateDescriptorSets(device, 1, &wds, 0, NULL);

   // ---- commands
   VkCommandPoolCreateInfo cpi = { .sType = VK_STRUCTURE_TYPE_COMMAND_POOL_CREATE_INFO, .queueFamilyIndex = family };
   VKOK(vkCreateCommandPool(device, &cpi, NULL, &cpool));
   VkCommandBufferAllocateInfo cbai = { .sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_ALLOCATE_INFO, .commandPool = cpool, .level = VK_COMMAND_BUFFER_LEVEL_PRIMARY, .commandBufferCount = 1 };
   VkCommandBuffer cb;
   VKOK(vkAllocateCommandBuffers(device, &cbai, &cb));
   VkCommandBufferBeginInfo cbbi = { .sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO };
   VKOK(vkBeginCommandBuffer(cb, &cbbi));
   vkCmdBindPipeline(cb, VK_PIPELINE_BIND_POINT_COMPUTE, pipe);
   vkCmdBindDescriptorSets(cb, VK_PIPELINE_BIND_POINT_COMPUTE, pl, 0, 1, &ds, 0, NULL);
   vkCmdDispatch(cb, 1, 1, 1);
   VKOK(vkEndCommandBuffer(cb));

   // ---- submit and wait: a fence
   VkFenceCreateInfo fci = { .sType = VK_STRUCTURE_TYPE_FENCE_CREATE_INFO };
   VKOK(vkCreateFence(device, &fci, NULL, &fence));
   VkSubmitInfo si = { .sType = VK_STRUCTURE_TYPE_SUBMIT_INFO, .commandBufferCount = 1, .pCommandBuffers = &cb };
   VKOK(vkQueueSubmit(queue, 1, &si, fence));
   r = vkWaitForFences(device, 1, &fence, VK_TRUE, 5000000000ull);
   CHECK(r == VK_SUCCESS, "the fence signals (%d)", (int)r);

   int executed = 1;
   for (uint32_t i = 0; i < N; i++) if (data[i] != i * 3u + 1234u) executed = 0;
   int untouched = 1;
   for (uint32_t i = 0; i < N; i++) if (data[i] != 0xdeadbeefu) untouched = 0;
   printf("VK dispatch result: %s\n", executed ? "EXECUTED" : untouched ? "not executed (software device)" : "WRONG DATA");
   CHECK(executed || untouched, "the buffer holds either the results or what we put there");
   // On the hardware the dispatch must have run: VK_PROBE_REQUIRE_EXEC=1 (the metal job) makes "not executed" a failure.
   if (getenv("VK_PROBE_REQUIRE_EXEC")) CHECK(executed, "the dispatch really ran on the GPU");

   // ---- a timeline semaphore
   VkSemaphoreTypeCreateInfo stci = { .sType = VK_STRUCTURE_TYPE_SEMAPHORE_TYPE_CREATE_INFO, .semaphoreType = VK_SEMAPHORE_TYPE_TIMELINE, .initialValue = 0 };
   VkSemaphoreCreateInfo sci = { .sType = VK_STRUCTURE_TYPE_SEMAPHORE_CREATE_INFO, .pNext = &stci };
   VKOK(vkCreateSemaphore(device, &sci, NULL, &tsem));
   uint64_t sig = 5;
   VkTimelineSemaphoreSubmitInfo tssi = { .sType = VK_STRUCTURE_TYPE_TIMELINE_SEMAPHORE_SUBMIT_INFO, .signalSemaphoreValueCount = 1, .pSignalSemaphoreValues = &sig };
   VkSubmitInfo si2 = { .sType = VK_STRUCTURE_TYPE_SUBMIT_INFO, .pNext = &tssi, .commandBufferCount = 1, .pCommandBuffers = &cb, .signalSemaphoreCount = 1, .pSignalSemaphores = &tsem };
   VKOK(vkQueueSubmit(queue, 1, &si2, VK_NULL_HANDLE));
   VkSemaphoreWaitInfo swi = { .sType = VK_STRUCTURE_TYPE_SEMAPHORE_WAIT_INFO, .semaphoreCount = 1, .pSemaphores = &tsem, .pValues = &sig };
   r = vkWaitSemaphores(device, &swi, 5000000000ull);
   CHECK(r == VK_SUCCESS, "the timeline reaches 5 (%d)", (int)r);
   uint64_t v = 0;
   VKOK(vkGetSemaphoreCounterValue(device, tsem, &v));
   CHECK(v >= 5, "counter value %llu", (unsigned long long)v);
   VkSemaphoreSignalInfo ssi = { .sType = VK_STRUCTURE_TYPE_SEMAPHORE_SIGNAL_INFO, .semaphore = tsem, .value = 9 };
   VKOK(vkSignalSemaphore(device, &ssi));
   VKOK(vkGetSemaphoreCounterValue(device, tsem, &v));
   CHECK(v == 9, "signalled from the CPU to 9 (%llu)", (unsigned long long)v);

   // ---- a binary semaphore ordering two submissions
   VkSemaphoreCreateInfo bsci = { .sType = VK_STRUCTURE_TYPE_SEMAPHORE_CREATE_INFO };
   VKOK(vkCreateSemaphore(device, &bsci, NULL, &bsem));
   VkSubmitInfo first = { .sType = VK_STRUCTURE_TYPE_SUBMIT_INFO, .commandBufferCount = 1, .pCommandBuffers = &cb, .signalSemaphoreCount = 1, .pSignalSemaphores = &bsem };
   VkPipelineStageFlags stage = VK_PIPELINE_STAGE_COMPUTE_SHADER_BIT;
   VkSubmitInfo second = { .sType = VK_STRUCTURE_TYPE_SUBMIT_INFO, .commandBufferCount = 1, .pCommandBuffers = &cb, .waitSemaphoreCount = 1, .pWaitSemaphores = &bsem, .pWaitDstStageMask = &stage };
   VKOK(vkResetFences(device, 1, &fence));
   VKOK(vkQueueSubmit(queue, 1, &first, VK_NULL_HANDLE));
   VKOK(vkQueueSubmit(queue, 1, &second, fence));
   r = vkWaitForFences(device, 1, &fence, VK_TRUE, 5000000000ull);
   CHECK(r == VK_SUCCESS, "the fence after a binary-semaphore chain signals (%d)", (int)r);
   VKOK(vkQueueWaitIdle(queue));
   VKOK(vkDeviceWaitIdle(device));

done:
   if (device) {
      PFN_vkDeviceWaitIdle wi = (PFN_vkDeviceWaitIdle)vkGetDeviceProcAddr(device, "vkDeviceWaitIdle");
      if (wi) wi(device);
      #define GONE(handle, fn, ...) do { PFN_##fn f_ = (PFN_##fn)vkGetDeviceProcAddr(device, #fn); if (handle && f_) f_(device, __VA_ARGS__); } while (0)
      GONE(bsem, vkDestroySemaphore, bsem, NULL);
      GONE(tsem, vkDestroySemaphore, tsem, NULL);
      GONE(fence, vkDestroyFence, fence, NULL);
      GONE(cpool, vkDestroyCommandPool, cpool, NULL);
      GONE(dpool, vkDestroyDescriptorPool, dpool, NULL);
      GONE(pipe, vkDestroyPipeline, pipe, NULL);
      GONE(pl, vkDestroyPipelineLayout, pl, NULL);
      GONE(dsl, vkDestroyDescriptorSetLayout, dsl, NULL);
      GONE(shader, vkDestroyShaderModule, shader, NULL);
      GONE(vbuf, vkDestroyBuffer, vbuf, NULL);
      GONE(buf, vkDestroyBuffer, buf, NULL);
      GONE(vmem, vkFreeMemory, vmem, NULL);
      GONE(mem, vkFreeMemory, mem, NULL);
      PFN_vkDestroyDevice dd = (PFN_vkDestroyDevice)vkGetDeviceProcAddr(device, "vkDestroyDevice");
      if (dd) dd(device, NULL);
   }
   if (instance) {
      PFN_vkDestroyInstance di = (PFN_vkDestroyInstance)vk_icdGetInstanceProcAddr(instance, "vkDestroyInstance");
      if (di) di(instance, NULL);
   }
   if (failures) { printf("VK PROBE FAILED (%d)\n", failures); return 1; }
   printf("VK PROBE DONE\n");
   return 0;
}

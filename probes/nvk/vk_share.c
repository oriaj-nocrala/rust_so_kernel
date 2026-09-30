/*
 * Memory and a timeline semaphore shared between two processes through Vulkan on constanos (G5 layer 2, docs/gpu/g5-graphics-stack-plan.md):
 * the parent allocates host-visible memory with an exportable handle (VK_KHR_external_memory_fd, opaque fd) and an exportable timeline
 * semaphore (VK_KHR_external_semaphore_fd), queues a GPU fill of a third of the memory that signals the semaphore to 1, sends both fds over
 * an AF_UNIX socket (SCM_RIGHTS) and then sits idle on the socket: no Vulkan call, so nothing of the parent's advances the semaphore. The
 * child has a Vulkan device of its own (a /dev/nvgpu session of its own), imports both (nvkmd_constanos' import_dma_buf -> NVG_IOC_BO_IMPORT,
 * import_opaque_fd -> NVG_IOC_SYNC_IMPORT), waits for the semaphore to reach 1 and only then reads what the parent's GPU wrote, queues its own
 * GPU fill of the middle third signalling 2, writes the last third with its CPU, and sits idle in turn. The parent waits for 2 on its own
 * semaphore (it is the same timeline) and checks all three thirds through its own mapping.
 *
 *   [0, 32K)    parent's GPU (vkCmdFillBuffer)  0xaaaa5555
 *   [32K, 64K)  child's GPU                     0x5555aaaa
 *   [64K, 96K)  child's CPU                     0x0f1e2d3c
 *
 * On the software device nothing executes: the GPU thirds are not checked (they are when VK_PROBE_REQUIRE_EXEC=1, as in vk_probe.c).
 * Built like the other probes (probes/nvk/build.py): ~/src/gpu-ref/nvk-probe/vk-share.
 */
#define VK_NO_PROTOTYPES
#include <vulkan/vulkan.h>

#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/uio.h>
#include <sys/wait.h>
#include <unistd.h>
#include <unwind.h>

static _Unwind_Reason_Code trace_cb(struct _Unwind_Context *c, void *arg) {
   (void)arg;
   printf("VK BT %#lx\n", (unsigned long)_Unwind_GetIP(c));
   return _URC_NO_REASON;
}

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

#define GLOBAL(name) PFN_##name name = (PFN_##name)vk_icdGetInstanceProcAddr(NULL, #name)
#define INST(name) PFN_##name name = (PFN_##name)vk_icdGetInstanceProcAddr(instance, #name)
#define DEV(name) PFN_##name name = (PFN_##name)vkGetDeviceProcAddr(device, #name)

#define SIZE (96u * 1024)
#define THIRD (32u * 1024)
#define W_PARENT_GPU 0xaaaa5555u
#define W_CHILD_GPU 0x5555aaaau
#define W_CHILD_CPU 0x0f1e2d3cu

static int failures;
#define CHECK(cond, ...) do { if (cond) printf("VK ok   %s\n", #cond); else { failures++; printf("VK FAIL %s (line %d): ", #cond, __LINE__); printf(__VA_ARGS__); printf("\n"); } } while (0)

static int send_fd(int sock, int what) {
   char data = 'x';
   struct iovec iov = { .iov_base = &data, .iov_len = 1 };
   char control[CMSG_SPACE(sizeof(int))];
   memset(control, 0, sizeof control);
   struct msghdr msg = { .msg_iov = &iov, .msg_iovlen = 1, .msg_control = control, .msg_controllen = sizeof control };
   struct cmsghdr *c = CMSG_FIRSTHDR(&msg);
   c->cmsg_level = SOL_SOCKET;
   c->cmsg_type = SCM_RIGHTS;
   c->cmsg_len = CMSG_LEN(sizeof(int));
   memcpy(CMSG_DATA(c), &what, sizeof(int));
   return sendmsg(sock, &msg, 0) == 1 ? 0 : -1;
}

static int recv_fd(int sock) {
   char data = 0;
   struct iovec iov = { .iov_base = &data, .iov_len = 1 };
   char control[CMSG_SPACE(sizeof(int))];
   memset(control, 0, sizeof control);
   struct msghdr msg = { .msg_iov = &iov, .msg_iovlen = 1, .msg_control = control, .msg_controllen = sizeof control };
   if (recvmsg(sock, &msg, 0) != 1) return -1;
   struct cmsghdr *c = CMSG_FIRSTHDR(&msg);
   int got = -1;
   if (c && c->cmsg_level == SOL_SOCKET && c->cmsg_type == SCM_RIGHTS) memcpy(&got, CMSG_DATA(c), sizeof(int));
   return got;
}

/* One process's Vulkan: instance, device, queue, a command pool. */
struct vk {
   VkInstance instance;
   VkPhysicalDevice pdev;
   VkDevice device;
   VkQueue queue;
   VkCommandPool pool;
   VkPhysicalDeviceMemoryProperties mp;
   PFN_vkGetDeviceProcAddr vkGetDeviceProcAddr;
   PFN_vkCreateBuffer vkCreateBuffer;
   PFN_vkGetBufferMemoryRequirements vkGetBufferMemoryRequirements;
   PFN_vkAllocateMemory vkAllocateMemory;
   PFN_vkBindBufferMemory vkBindBufferMemory;
   PFN_vkMapMemory vkMapMemory;
   PFN_vkGetMemoryFdKHR vkGetMemoryFdKHR;
   PFN_vkAllocateCommandBuffers vkAllocateCommandBuffers;
   PFN_vkBeginCommandBuffer vkBeginCommandBuffer;
   PFN_vkEndCommandBuffer vkEndCommandBuffer;
   PFN_vkCmdFillBuffer vkCmdFillBuffer;
   PFN_vkCreateSemaphore vkCreateSemaphore;
   PFN_vkGetSemaphoreFdKHR vkGetSemaphoreFdKHR;
   PFN_vkImportSemaphoreFdKHR vkImportSemaphoreFdKHR;
   PFN_vkWaitSemaphores vkWaitSemaphores;
   PFN_vkCreateFence vkCreateFence;
   PFN_vkQueueSubmit vkQueueSubmit;
   PFN_vkWaitForFences vkWaitForFences;
};

static int vk_setup(struct vk *v, const char *who) {
   memset(v, 0, sizeof *v);
   GLOBAL(vkCreateInstance);
   if (!vkCreateInstance) return -1;
   VkApplicationInfo app = { .sType = VK_STRUCTURE_TYPE_APPLICATION_INFO, .pApplicationName = "vk_share", .apiVersion = VK_API_VERSION_1_3 };
   VkInstanceCreateInfo ici = { .sType = VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO, .pApplicationInfo = &app };
   if (vkCreateInstance(&ici, NULL, &v->instance) != VK_SUCCESS) return -2;
   VkInstance instance = v->instance;
   INST(vkEnumeratePhysicalDevices);
   INST(vkGetPhysicalDeviceQueueFamilyProperties);
   INST(vkGetPhysicalDeviceMemoryProperties);
   INST(vkCreateDevice);
   v->vkGetDeviceProcAddr = (PFN_vkGetDeviceProcAddr)vk_icdGetInstanceProcAddr(instance, "vkGetDeviceProcAddr");
   uint32_t n = 1;
   if (vkEnumeratePhysicalDevices(instance, &n, &v->pdev) < 0 || n != 1) return -3;
   uint32_t nq = 0;
   vkGetPhysicalDeviceQueueFamilyProperties(v->pdev, &nq, NULL);
   VkQueueFamilyProperties qf[16];
   if (nq > 16) nq = 16;
   vkGetPhysicalDeviceQueueFamilyProperties(v->pdev, &nq, qf);
   int family = -1;
   for (uint32_t i = 0; i < nq; i++) {
      // a compute-only family first, as vk_probe does (its contexts ask for the engines the hardware device serves)
      if ((qf[i].queueFlags & VK_QUEUE_COMPUTE_BIT) && !(qf[i].queueFlags & VK_QUEUE_GRAPHICS_BIT) && (family < 0 || (qf[family].queueFlags & VK_QUEUE_GRAPHICS_BIT))) family = (int)i;
      else if (family < 0 && (qf[i].queueFlags & VK_QUEUE_COMPUTE_BIT)) family = (int)i;
   }
   if (family < 0) return -4;
   vkGetPhysicalDeviceMemoryProperties(v->pdev, &v->mp);
   float prio = 1.0f;
   VkDeviceQueueCreateInfo qci = { .sType = VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO, .queueFamilyIndex = family, .queueCount = 1, .pQueuePriorities = &prio };
   const char *exts[] = { "VK_KHR_external_memory_fd", "VK_KHR_external_semaphore_fd" };
   VkPhysicalDeviceVulkan12Features f12 = { .sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_VULKAN_1_2_FEATURES, .timelineSemaphore = VK_TRUE };
   VkDeviceCreateInfo dci = { .sType = VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO, .pNext = &f12, .queueCreateInfoCount = 1, .pQueueCreateInfos = &qci, .enabledExtensionCount = 2, .ppEnabledExtensionNames = exts };
   if (vkCreateDevice(v->pdev, &dci, NULL, &v->device) != VK_SUCCESS) return -5;
   VkDevice device = v->device;
   PFN_vkGetDeviceProcAddr vkGetDeviceProcAddr = v->vkGetDeviceProcAddr;
   DEV(vkGetDeviceQueue);
   DEV(vkCreateCommandPool);
   v->vkCreateBuffer = (PFN_vkCreateBuffer)vkGetDeviceProcAddr(device, "vkCreateBuffer");
   v->vkGetBufferMemoryRequirements = (PFN_vkGetBufferMemoryRequirements)vkGetDeviceProcAddr(device, "vkGetBufferMemoryRequirements");
   v->vkAllocateMemory = (PFN_vkAllocateMemory)vkGetDeviceProcAddr(device, "vkAllocateMemory");
   v->vkBindBufferMemory = (PFN_vkBindBufferMemory)vkGetDeviceProcAddr(device, "vkBindBufferMemory");
   v->vkMapMemory = (PFN_vkMapMemory)vkGetDeviceProcAddr(device, "vkMapMemory");
   v->vkGetMemoryFdKHR = (PFN_vkGetMemoryFdKHR)vkGetDeviceProcAddr(device, "vkGetMemoryFdKHR");
   v->vkAllocateCommandBuffers = (PFN_vkAllocateCommandBuffers)vkGetDeviceProcAddr(device, "vkAllocateCommandBuffers");
   v->vkBeginCommandBuffer = (PFN_vkBeginCommandBuffer)vkGetDeviceProcAddr(device, "vkBeginCommandBuffer");
   v->vkEndCommandBuffer = (PFN_vkEndCommandBuffer)vkGetDeviceProcAddr(device, "vkEndCommandBuffer");
   v->vkCmdFillBuffer = (PFN_vkCmdFillBuffer)vkGetDeviceProcAddr(device, "vkCmdFillBuffer");
   v->vkCreateSemaphore = (PFN_vkCreateSemaphore)vkGetDeviceProcAddr(device, "vkCreateSemaphore");
   v->vkGetSemaphoreFdKHR = (PFN_vkGetSemaphoreFdKHR)vkGetDeviceProcAddr(device, "vkGetSemaphoreFdKHR");
   v->vkImportSemaphoreFdKHR = (PFN_vkImportSemaphoreFdKHR)vkGetDeviceProcAddr(device, "vkImportSemaphoreFdKHR");
   v->vkWaitSemaphores = (PFN_vkWaitSemaphores)vkGetDeviceProcAddr(device, "vkWaitSemaphores");
   v->vkCreateFence = (PFN_vkCreateFence)vkGetDeviceProcAddr(device, "vkCreateFence");
   v->vkQueueSubmit = (PFN_vkQueueSubmit)vkGetDeviceProcAddr(device, "vkQueueSubmit");
   v->vkWaitForFences = (PFN_vkWaitForFences)vkGetDeviceProcAddr(device, "vkWaitForFences");
   vkGetDeviceQueue(device, family, 0, &v->queue);
   VkCommandPoolCreateInfo cpi = { .sType = VK_STRUCTURE_TYPE_COMMAND_POOL_CREATE_INFO, .queueFamilyIndex = family };
   if (vkCreateCommandPool(device, &cpi, NULL, &v->pool) != VK_SUCCESS) return -6;
   printf("VK %s: device ready (queue family %d)\n", who, family);
   return 0;
}

/* A buffer of SIZE bytes whose memory is host-visible, coherent and shareable by an opaque fd: allocated (export) or imported from `import_fd`. */
static int make_shared_buffer(struct vk *v, int import_fd, VkBuffer *buf_out, VkDeviceMemory *mem_out, uint32_t **map_out) {
   VkExternalMemoryBufferCreateInfo ebi = { .sType = VK_STRUCTURE_TYPE_EXTERNAL_MEMORY_BUFFER_CREATE_INFO,
                                            .handleTypes = VK_EXTERNAL_MEMORY_HANDLE_TYPE_OPAQUE_FD_BIT };
   VkBufferCreateInfo bci = { .sType = VK_STRUCTURE_TYPE_BUFFER_CREATE_INFO, .pNext = &ebi, .size = SIZE,
                              .usage = VK_BUFFER_USAGE_TRANSFER_DST_BIT | VK_BUFFER_USAGE_TRANSFER_SRC_BIT, .sharingMode = VK_SHARING_MODE_EXCLUSIVE };
   if (v->vkCreateBuffer(v->device, &bci, NULL, buf_out) != VK_SUCCESS) return -1;
   VkMemoryRequirements req;
   v->vkGetBufferMemoryRequirements(v->device, *buf_out, &req);
   int t = -1;
   for (uint32_t i = 0; i < v->mp.memoryTypeCount; i++) {
      VkMemoryPropertyFlags want = VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT | VK_MEMORY_PROPERTY_HOST_COHERENT_BIT;
      if ((req.memoryTypeBits & (1u << i)) && (v->mp.memoryTypes[i].propertyFlags & want) == want) { t = (int)i; break; }
   }
   if (t < 0) return -2;
   VkExportMemoryAllocateInfo exp = { .sType = VK_STRUCTURE_TYPE_EXPORT_MEMORY_ALLOCATE_INFO, .handleTypes = VK_EXTERNAL_MEMORY_HANDLE_TYPE_OPAQUE_FD_BIT };
   VkImportMemoryFdInfoKHR imp = { .sType = VK_STRUCTURE_TYPE_IMPORT_MEMORY_FD_INFO_KHR, .handleType = VK_EXTERNAL_MEMORY_HANDLE_TYPE_OPAQUE_FD_BIT, .fd = import_fd };
   VkMemoryAllocateInfo mai = { .sType = VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO, .pNext = import_fd >= 0 ? (void *)&imp : (void *)&exp,
                                .allocationSize = req.size, .memoryTypeIndex = t };
   if (v->vkAllocateMemory(v->device, &mai, NULL, mem_out) != VK_SUCCESS) return -3;
   if (v->vkBindBufferMemory(v->device, *buf_out, *mem_out, 0) != VK_SUCCESS) return -4;
   if (v->vkMapMemory(v->device, *mem_out, 0, VK_WHOLE_SIZE, 0, (void **)map_out) != VK_SUCCESS) return -5;
   return 0;
}

/* vkCmdFillBuffer of [offset, offset + THIRD) with `word`, submitted and waited for. */
static int gpu_fill(struct vk *v, VkBuffer buf, uint32_t offset, uint32_t word) {
   VkCommandBufferAllocateInfo cai = { .sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_ALLOCATE_INFO, .commandPool = v->pool, .level = VK_COMMAND_BUFFER_LEVEL_PRIMARY, .commandBufferCount = 1 };
   VkCommandBuffer cb;
   if (v->vkAllocateCommandBuffers(v->device, &cai, &cb) != VK_SUCCESS) return -1;
   VkCommandBufferBeginInfo bi = { .sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO, .flags = VK_COMMAND_BUFFER_USAGE_ONE_TIME_SUBMIT_BIT };
   if (v->vkBeginCommandBuffer(cb, &bi) != VK_SUCCESS) return -2;
   v->vkCmdFillBuffer(cb, buf, offset, THIRD, word);
   if (v->vkEndCommandBuffer(cb) != VK_SUCCESS) return -3;
   VkFenceCreateInfo fci = { .sType = VK_STRUCTURE_TYPE_FENCE_CREATE_INFO };
   VkFence fence;
   if (v->vkCreateFence(v->device, &fci, NULL, &fence) != VK_SUCCESS) return -4;
   VkSubmitInfo si = { .sType = VK_STRUCTURE_TYPE_SUBMIT_INFO, .commandBufferCount = 1, .pCommandBuffers = &cb };
   if (v->vkQueueSubmit(v->queue, 1, &si, fence) != VK_SUCCESS) return -5;
   if (v->vkWaitForFences(v->device, 1, &fence, VK_TRUE, 10000000000ull) != VK_SUCCESS) return -6;
   return 0;
}

/* A timeline semaphore (initial value 0) whose payload can be shared by an opaque fd: created exportable, or created and then given the
 * payload of `import_fd`. */
static int make_timeline(struct vk *v, int import_fd, VkSemaphore *out) {
   VkSemaphoreTypeCreateInfo ti = { .sType = VK_STRUCTURE_TYPE_SEMAPHORE_TYPE_CREATE_INFO, .semaphoreType = VK_SEMAPHORE_TYPE_TIMELINE, .initialValue = 0 };
   VkExportSemaphoreCreateInfo ei = { .sType = VK_STRUCTURE_TYPE_EXPORT_SEMAPHORE_CREATE_INFO, .pNext = &ti, .handleTypes = VK_EXTERNAL_SEMAPHORE_HANDLE_TYPE_OPAQUE_FD_BIT };
   VkSemaphoreCreateInfo ci = { .sType = VK_STRUCTURE_TYPE_SEMAPHORE_CREATE_INFO, .pNext = import_fd >= 0 ? (void *)&ti : (void *)&ei };
   if (v->vkCreateSemaphore(v->device, &ci, NULL, out) != VK_SUCCESS) return -1;
   if (import_fd >= 0) {
      VkImportSemaphoreFdInfoKHR ii = { .sType = VK_STRUCTURE_TYPE_IMPORT_SEMAPHORE_FD_INFO_KHR, .semaphore = *out,
                                        .handleType = VK_EXTERNAL_SEMAPHORE_HANDLE_TYPE_OPAQUE_FD_BIT, .fd = import_fd };
      if (v->vkImportSemaphoreFdKHR(v->device, &ii) != VK_SUCCESS) return -2;
   }
   return 0;
}

/* Queue a fill of [offset, offset + THIRD) that signals `sem` to `value`, and return at once (nothing waits for it here). */
static int gpu_fill_signal(struct vk *v, VkBuffer buf, uint32_t offset, uint32_t word, VkSemaphore sem, uint64_t value) {
   VkCommandBufferAllocateInfo cai = { .sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_ALLOCATE_INFO, .commandPool = v->pool, .level = VK_COMMAND_BUFFER_LEVEL_PRIMARY, .commandBufferCount = 1 };
   VkCommandBuffer cb;
   if (v->vkAllocateCommandBuffers(v->device, &cai, &cb) != VK_SUCCESS) return -1;
   VkCommandBufferBeginInfo bi = { .sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO, .flags = VK_COMMAND_BUFFER_USAGE_ONE_TIME_SUBMIT_BIT };
   if (v->vkBeginCommandBuffer(cb, &bi) != VK_SUCCESS) return -2;
   v->vkCmdFillBuffer(cb, buf, offset, THIRD, word);
   if (v->vkEndCommandBuffer(cb) != VK_SUCCESS) return -3;
   VkTimelineSemaphoreSubmitInfo tsi = { .sType = VK_STRUCTURE_TYPE_TIMELINE_SEMAPHORE_SUBMIT_INFO, .signalSemaphoreValueCount = 1, .pSignalSemaphoreValues = &value };
   VkSubmitInfo si = { .sType = VK_STRUCTURE_TYPE_SUBMIT_INFO, .pNext = &tsi, .commandBufferCount = 1, .pCommandBuffers = &cb, .signalSemaphoreCount = 1, .pSignalSemaphores = &sem };
   return v->vkQueueSubmit(v->queue, 1, &si, VK_NULL_HANDLE) == VK_SUCCESS ? 0 : -4;
}

static int wait_timeline(struct vk *v, VkSemaphore sem, uint64_t value) {
   VkSemaphoreWaitInfo wi = { .sType = VK_STRUCTURE_TYPE_SEMAPHORE_WAIT_INFO, .semaphoreCount = 1, .pSemaphores = &sem, .pValues = &value };
   return v->vkWaitSemaphores(v->device, &wi, 10000000000ull) == VK_SUCCESS ? 0 : -1;
}

static int region_is(const uint32_t *m, uint32_t third, uint32_t word) {
   for (uint32_t i = 0; i < THIRD / 4; i++)
      if (m[third * (THIRD / 4) + i] != word) return 0;
   return 1;
}

static int require_exec;

static int child(int sock) {
   int mem_fd = recv_fd(sock), sem_fd = recv_fd(sock);
   if (mem_fd < 0 || sem_fd < 0) { printf("VK child: descriptors did not arrive (%d %d)\n", mem_fd, sem_fd); return 10; }
   struct vk v;
   int r = vk_setup(&v, "child");
   if (r) { printf("VK child: setup failed (%d)\n", r); return 11; }
   VkBuffer buf;
   VkDeviceMemory mem;
   uint32_t *m = NULL;
   r = make_shared_buffer(&v, mem_fd, &buf, &mem, &m);
   CHECK(r == 0, "the child imports the parent's memory (step %d)", r);
   if (r) return 12;
   VkSemaphore sem;
   r = make_timeline(&v, sem_fd, &sem);
   CHECK(r == 0, "the child imports the parent's timeline semaphore (step %d)", r);
   if (r) return 13;
   // the parent is idle on its socket: only reading the shared timeline can tell that its GPU work is complete
   r = wait_timeline(&v, sem, 1);
   CHECK(r == 0, "the child waits for the parent's GPU work on the shared timeline (value 1)");
   if (r) return 14;
   if (require_exec) CHECK(region_is(m, 0, W_PARENT_GPU), "and then the child's CPU sees what the parent's GPU wrote (first word %#x)", m[0]);
   r = gpu_fill_signal(&v, buf, THIRD, W_CHILD_GPU, sem, 2);
   CHECK(r == 0, "the child queues its GPU fill of the middle third, signalling 2 (step %d)", r);
   for (uint32_t i = 0; i < THIRD / 4; i++) m[2 * (THIRD / 4) + i] = W_CHILD_CPU;
   char done = 'D';
   if (write(sock, &done, 1) != 1) return 15;
   char go = 0;
   if (read(sock, &go, 1) != 1) return 16;   // idle until the parent has seen the result
   return failures ? 17 : 0;
}

int main(void) {
   setvbuf(stdout, NULL, _IONBF, 0);
   require_exec = getenv("VK_PROBE_REQUIRE_EXEC") != NULL;
   int sv[2];
   if (socketpair(AF_UNIX, SOCK_STREAM, 0, sv) != 0) { printf("VK FAIL socketpair\n"); return 1; }
   pid_t kid = fork();
   if (kid == 0) {
      close(sv[0]);
      _exit(child(sv[1]));
   }
   close(sv[1]);

   struct vk v;
   int r = vk_setup(&v, "parent");
   CHECK(r == 0, "the parent's Vulkan (%d)", r);
   if (r) return 1;
   VkBuffer buf;
   VkDeviceMemory mem;
   uint32_t *m = NULL;
   r = make_shared_buffer(&v, -1, &buf, &mem, &m);
   CHECK(r == 0, "exportable host-visible memory and a mapping (step %d)", r);
   if (r) return 1;
   memset(m, 0x11, SIZE);
   VkSemaphore sem;
   r = make_timeline(&v, -1, &sem);
   CHECK(r == 0, "an exportable timeline semaphore (step %d)", r);
   if (r) return 1;
   r = gpu_fill_signal(&v, buf, 0, W_PARENT_GPU, sem, 1);
   CHECK(r == 0, "the parent queues its GPU fill of the first third, signalling 1 (step %d)", r);
   int mem_fd = -1, sem_fd = -1;
   VkMemoryGetFdInfoKHR gi = { .sType = VK_STRUCTURE_TYPE_MEMORY_GET_FD_INFO_KHR, .memory = mem, .handleType = VK_EXTERNAL_MEMORY_HANDLE_TYPE_OPAQUE_FD_BIT };
   r = v.vkGetMemoryFdKHR(v.device, &gi, &mem_fd);
   CHECK(r == VK_SUCCESS && mem_fd >= 0, "vkGetMemoryFdKHR gives a descriptor (%d, fd %d)", r, mem_fd);
   VkSemaphoreGetFdInfoKHR sgi = { .sType = VK_STRUCTURE_TYPE_SEMAPHORE_GET_FD_INFO_KHR, .semaphore = sem, .handleType = VK_EXTERNAL_SEMAPHORE_HANDLE_TYPE_OPAQUE_FD_BIT };
   r = v.vkGetSemaphoreFdKHR(v.device, &sgi, &sem_fd);
   CHECK(r == VK_SUCCESS && sem_fd >= 0, "vkGetSemaphoreFdKHR gives a descriptor (%d, fd %d)", r, sem_fd);
   if (mem_fd < 0 || sem_fd < 0) return 1;
   CHECK(send_fd(sv[0], mem_fd) == 0 && send_fd(sv[0], sem_fd) == 0, "both descriptors are sent to the child");
   close(mem_fd);
   close(sem_fd);
   // from here the parent calls nothing of Vulkan until the child says its work is queued: it is idle
   char done = 0;
   CHECK(read(sv[0], &done, 1) == 1 && done == 'D', "the child reports that its work is queued");
   CHECK(wait_timeline(&v, sem, 2) == 0, "the parent sees the child's work complete on its own semaphore (value 2)");
   char go = 'k';
   CHECK(write(sv[0], &go, 1) == 1, "the child is released");
   int st = 0;
   waitpid(kid, &st, 0);
   int code = WIFEXITED(st) ? WEXITSTATUS(st) : -1;
   CHECK(code == 0, "the child exited cleanly (%d: 10 fds, 11 setup, 12 memory import, 13 semaphore import, 14 never saw 1, 15-16 socket, 17 its checks failed)", code);
   if (require_exec) {
      CHECK(region_is(m, 0, W_PARENT_GPU), "the first third is still the parent's GPU's (first word %#x)", m[0]);
      CHECK(region_is(m, 1, W_CHILD_GPU), "the middle third is what the child's GPU wrote (first word %#x)", m[THIRD / 4]);
   } else {
      printf("VK skip the GPU thirds (software device)\n");
   }
   CHECK(region_is(m, 2, W_CHILD_CPU), "the last third is what the child's CPU wrote (first word %#x)", m[2 * (THIRD / 4)]);
   if (failures) {
      printf("VK SHARE FAILED (%d)\n", failures);
      return 1;
   }
   printf("VK SHARE DONE\n");
   return 0;
}

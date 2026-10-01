/*
 * vk_comp: the GPU compositor (G5 layer 4, slice 3c; docs/gpu/g5-graphics-stack-plan.md). The machine around the window manager
 * (gui_capi: the `gui` crate) and the renderer (comp_render.h): it owns the screen (the WSI's direct path: a headless surface is the display),
 * listens on /tmp/gui-0, reads the keyboard and the mouse, imports the GPU buffers its clients export (the clients are programs that present
 * through NVK's windowed WSI: vk_window, snake3d) and composes every frame on the GPU. Clients that draw with the CPU (term, panel, cpumon) keep
 * working: their pixels are uploaded when they change. The CPU compositor (userspace/src/bin/compositor.rs) stays the one for a machine
 * without the GPU driver.
 *
 *   vk_comp [prog...]        starts each prog once the socket listens (default: panel, if it exists); Ctrl+Alt+Backspace quits
 *   COMP_HEADLESS=1          no display (QEMU's software device): the surface has no size, render 640x360 and present nothing visible
 *   COMP_NO_INPUT=1          do not open (or grab) the input devices
 *   COMP_SECONDS=<n>         quit after n seconds (unattended runs); SIGTERM quits too
 *   COMP_SOCKET=<path>       the socket (default /tmp/gui-0)
 */
#define VK_NO_PROTOTYPES
#include <vulkan/vulkan.h>

#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/un.h>
#include <sys/uio.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>
#include <unwind.h>

#include "comp_render.h"

static _Unwind_Reason_Code trace_cb(struct _Unwind_Context *c, void *arg) {
   (void)arg;
   printf("COMP BT %#lx\n", (unsigned long)_Unwind_GetIP(c));
   return _URC_NO_REASON;
}

void __assert_fail(const char *expr, const char *file, int line, const char *func) {
   printf("COMP ASSERT %s (%s: %s: %d)\n", expr, file, func, line);
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

static int failures;
#define VKOK(call) do { VkResult r_ = (call); if (r_ != VK_SUCCESS) { failures++; printf("COMP FAIL %s -> %d (line %d)\n", #call, (int)r_, __LINE__); goto done; } } while (0)

#define MAX_SC_IMAGES 8
#define MAX_CLIENTS 32
#define EVIOCGRAB 0x40044590UL
#define EV_SYN 0
#define EV_KEY 1
#define EV_REL 2
#define REL_X 0
#define REL_Y 1

static volatile sig_atomic_t quit_flag;
static void on_term(int sig) { (void)sig; quit_flag = 1; }

static uint64_t now_ms(void) {
   struct timespec t;
   clock_gettime(CLOCK_MONOTONIC, &t);
   return (uint64_t)t.tv_sec * 1000 + t.tv_nsec / 1000000;
}

/* ---- clients ------------------------------------------------------------------------------------------------------------------------ */

static gui_comp *gui;
static int client_fd[MAX_CLIENTS + 1];      /* by the window manager's client id (small integers, from 1) */
static int client_live[MAX_CLIENTS + 1];
static unsigned clients_seen;

static void drop_client(uint32_t id) {
   if (id > MAX_CLIENTS || !client_live[id]) return;
   close(client_fd[id]);
   client_live[id] = 0;
   gui_remove_client(gui, id);
   printf("COMP client %u gone\n", id);
}

/* What the window manager queued: events to the clients (never blocking on one: a client that does not read is dropped), the
 * disconnects it asked for, the descriptors it is done with. */
static void flush_outputs(void) {
   uint8_t buf[4096];
   uint32_t who;
   size_t len;
   while ((len = gui_pop_event(gui, &who, buf, sizeof(buf))) > 0) {
      if (who > MAX_CLIENTS || !client_live[who]) continue;
      if (send(client_fd[who], buf, len, MSG_DONTWAIT | MSG_NOSIGNAL) != (long)len) {
         printf("COMP client %u is not reading its events: dropped\n", who);
         drop_client(who);
      }
   }
   uint32_t c;
   while (gui_pop_disconnect(gui, &c)) {
      printf("COMP client %u disconnected for a protocol error\n", c);
      drop_client(c);
   }
   int32_t fd;
   while (gui_pop_fd_to_close(gui, &fd)) close(fd);
}

static void client_readable(uint32_t id) {
   for (;;) {
      uint8_t buf[4096];
      char ctl[CMSG_SPACE(sizeof(int) * 8)];
      struct iovec iov = { buf, sizeof(buf) };
      struct msghdr mh;
      memset(&mh, 0, sizeof(mh));
      mh.msg_iov = &iov;
      mh.msg_iovlen = 1;
      mh.msg_control = ctl;
      mh.msg_controllen = sizeof(ctl);
      long n = recvmsg(client_fd[id], &mh, MSG_DONTWAIT);
      int32_t fds[8];
      size_t nfds = 0;
      if (n >= 0) {
         for (struct cmsghdr *c = CMSG_FIRSTHDR(&mh); c; c = CMSG_NXTHDR(&mh, c)) {
            if (c->cmsg_level == SOL_SOCKET && c->cmsg_type == SCM_RIGHTS) {
               size_t cnt = (c->cmsg_len - CMSG_LEN(0)) / sizeof(int);
               for (size_t i = 0; i < cnt && nfds < 8; i++) memcpy(&fds[nfds++], CMSG_DATA(c) + i * sizeof(int), sizeof(int));
            }
         }
      }
      if (n == 0 && nfds == 0) { drop_client(id); return; }
      if (n < 0) {
         if (errno == EINTR) continue;
         return;   /* EAGAIN: everything was read */
      }
      gui_client_data(gui, id, buf, (size_t)n, fds, nfds);
      flush_outputs();
      if (!client_live[id]) return;
   }
}

/* ---- programs ------------------------------------------------------------------------------------------------------------------------ */

/* Starts `cmd` (a program name looked up in /bin and /mnt/bin, or a path; arguments split on spaces) in a group of its own. */
static pid_t start_program(const char *cmd, const char *socket_path) {
   char copy[256];
   strncpy(copy, cmd, sizeof(copy) - 1);
   copy[sizeof(copy) - 1] = 0;
   char *argv[16];
   int argc = 0;
   for (char *p = strtok(copy, " "); p && argc < 15; p = strtok(NULL, " ")) argv[argc++] = p;
   argv[argc] = NULL;
   if (argc == 0) return -1;
   char path[300];
   if (strchr(argv[0], '/')) {
      snprintf(path, sizeof(path), "%s", argv[0]);
   } else {
      snprintf(path, sizeof(path), "/bin/%s", argv[0]);
      if (access(path, X_OK) != 0) snprintf(path, sizeof(path), "/mnt/bin/%s", argv[0]);
   }
   if (access(path, X_OK) != 0) return -1;
   pid_t pid = fork();
   if (pid == 0) {
      setpgid(0, 0);
      signal(SIGINT, SIG_DFL);
      signal(SIGTSTP, SIG_DFL);
      signal(SIGTERM, SIG_DFL);
      setenv("GUI_DISPLAY", socket_path, 1);
      execv(path, argv);
      _exit(127);
   }
   return pid;
}

/* ---- input -------------------------------------------------------------------------------------------------------------------------- */

/* Drains an evdev fd (24-byte records), calling f(type, code, value). */
static void read_input(int fd, void (*f)(uint16_t, uint16_t, int32_t)) {
   for (int i = 0; i < 256; i++) {
      uint8_t rec[24];
      if (read(fd, rec, sizeof(rec)) != (long)sizeof(rec)) return;
      uint16_t type, code;
      int32_t value;
      memcpy(&type, rec + 16, 2);
      memcpy(&code, rec + 18, 2);
      memcpy(&value, rec + 20, 4);
      f(type, code, value);
   }
}

static int mdx, mdy;
static void on_kbd(uint16_t type, uint16_t code, int32_t value) {
   if (type == EV_KEY) gui_key(gui, code, value != 0);
}
static void on_mouse(uint16_t type, uint16_t code, int32_t value) {
   if (type == EV_REL && code == REL_X) mdx += value;
   else if (type == EV_REL && code == REL_Y) mdy -= value;   /* PS/2: positive is up; the screen's is down */
   else if (type == EV_KEY) gui_pointer_button(gui, code, value != 0);
   else if (type == EV_SYN) {
      if (mdx || mdy) gui_pointer_motion(gui, mdx, mdy);
      mdx = mdy = 0;
   }
}

/* ---- the program --------------------------------------------------------------------------------------------------------------------- */

int main(int argc, char **argv) {
   setvbuf(stdout, NULL, _IONBF, 0);
   signal(SIGINT, SIG_IGN);    /* a ^C or ^Z typed into a window is for that window (see the CPU compositor) */
   signal(SIGTSTP, SIG_IGN);
   signal(SIGPIPE, SIG_IGN);
   struct sigaction sa;
   memset(&sa, 0, sizeof(sa));
   sa.sa_handler = on_term;
   sigaction(SIGTERM, &sa, NULL);
   const char *e;
   const int headless = getenv("COMP_HEADLESS") != NULL, no_input = getenv("COMP_NO_INPUT") != NULL;
   const unsigned seconds = (e = getenv("COMP_SECONDS")) ? (unsigned)atoi(e) : 0;
   const char *socket_path = (e = getenv("COMP_SOCKET")) ? e : "/tmp/gui-0";

   VkInstance instance = VK_NULL_HANDLE;
   VkDevice device = VK_NULL_HANDLE;
   VkSurfaceKHR surface = VK_NULL_HANDLE;
   VkSwapchainKHR swapchain = VK_NULL_HANDLE;
   VkImage sc_images[MAX_SC_IMAGES];
   VkImageView sc_views[MAX_SC_IMAGES] = { VK_NULL_HANDLE };
   VkSemaphore render_sem[MAX_SC_IMAGES] = { VK_NULL_HANDLE }, acquire_sem = VK_NULL_HANDLE;
   uint32_t sc_count = 0;
   struct comp comp;
   int comp_ready = 0, lfd = -1, kbd = -1, mouse = -1;
   PFN_vkGetDeviceProcAddr vkGetDeviceProcAddr = NULL;
   uint32_t SW = 640, SH = 360;
   uint64_t frames = 0;

   GLOBAL(vkCreateInstance);
   if (!vkCreateInstance) return 1;
   VkApplicationInfo app = { .sType = VK_STRUCTURE_TYPE_APPLICATION_INFO, .pApplicationName = "vk_comp", .apiVersion = VK_API_VERSION_1_3 };
   static const char *const instance_exts[] = { "VK_KHR_surface", "VK_EXT_headless_surface" };
   VkInstanceCreateInfo ici = { .sType = VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO, .pApplicationInfo = &app, .enabledExtensionCount = 2, .ppEnabledExtensionNames = instance_exts };
   VKOK(vkCreateInstance(&ici, NULL, &instance));
   INST(vkEnumeratePhysicalDevices);
   INST(vkGetPhysicalDeviceQueueFamilyProperties);
   INST(vkGetPhysicalDeviceMemoryProperties);
   INST(vkCreateDevice);
   INST(vkCreateHeadlessSurfaceEXT);
   INST(vkGetPhysicalDeviceSurfaceSupportKHR);
   INST(vkGetPhysicalDeviceSurfaceCapabilitiesKHR);
   INST(vkDestroySurfaceKHR);
   INST(vkDestroyInstance);
   vkGetDeviceProcAddr = (PFN_vkGetDeviceProcAddr)vk_icdGetInstanceProcAddr(instance, "vkGetDeviceProcAddr");

   uint32_t npd = 1;
   VkPhysicalDevice pdev = VK_NULL_HANDLE;
   VkResult er = vkEnumeratePhysicalDevices(instance, &npd, &pdev);
   if ((er != VK_SUCCESS && er != VK_INCOMPLETE) || !pdev) { printf("COMP FAIL no physical device: is gpu=uapi on, and /dev/nvgpu free?\n"); failures++; goto done; }
   uint32_t nq = 16;
   VkQueueFamilyProperties qf[16];
   vkGetPhysicalDeviceQueueFamilyProperties(pdev, &nq, qf);
   int family = -1;
   for (uint32_t i = 0; i < nq && family < 0; i++) if (qf[i].queueFlags & VK_QUEUE_GRAPHICS_BIT) family = (int)i;
   if (family < 0) { printf("COMP FAIL no graphics queue family\n"); failures++; goto done; }
   VkPhysicalDeviceMemoryProperties mp;
   vkGetPhysicalDeviceMemoryProperties(pdev, &mp);
   float prio = 1.0f;
   VkDeviceQueueCreateInfo qci = { .sType = VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO, .queueFamilyIndex = (uint32_t)family, .queueCount = 1, .pQueuePriorities = &prio };
   static const char *const device_exts[] = { "VK_KHR_swapchain", "VK_KHR_external_memory_fd" };
   VkPhysicalDeviceVulkan13Features f13 = { .sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_VULKAN_1_3_FEATURES, .dynamicRendering = VK_TRUE };
   VkDeviceCreateInfo dci = { .sType = VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO, .pNext = &f13, .queueCreateInfoCount = 1, .pQueueCreateInfos = &qci,
                              .enabledExtensionCount = 2, .ppEnabledExtensionNames = device_exts };
   VKOK(vkCreateDevice(pdev, &dci, NULL, &device));
   DEV(vkCreateSwapchainKHR); DEV(vkDestroySwapchainKHR); DEV(vkGetSwapchainImagesKHR); DEV(vkAcquireNextImageKHR); DEV(vkQueuePresentKHR);
   DEV(vkCreateSemaphore); DEV(vkDestroySemaphore); DEV(vkCreateImageView); DEV(vkDestroyImageView); DEV(vkDestroyDevice);
   DEV(vkGetDeviceQueue);
   VkQueue queue;
   vkGetDeviceQueue(device, (uint32_t)family, 0, &queue);

   /* the screen: a headless surface is the display when the driver has one (the WSI's direct path) */
   VkHeadlessSurfaceCreateInfoEXT hsci = { .sType = VK_STRUCTURE_TYPE_HEADLESS_SURFACE_CREATE_INFO_EXT };
   VKOK(vkCreateHeadlessSurfaceEXT(instance, &hsci, NULL, &surface));
   VkBool32 supported = VK_FALSE;
   VKOK(vkGetPhysicalDeviceSurfaceSupportKHR(pdev, (uint32_t)family, surface, &supported));
   VkSurfaceCapabilitiesKHR caps;
   VKOK(vkGetPhysicalDeviceSurfaceCapabilitiesKHR(pdev, surface, &caps));
   const int has_display = caps.currentExtent.width != 0xffffffffu && caps.currentExtent.width != 0;
   if (!has_display && !headless) { printf("COMP FAIL no display behind the surface: is gpu=uapi on, and nothing else holding the screen? (COMP_HEADLESS=1 to run without one)\n"); failures++; goto done; }
   if (has_display) { SW = caps.currentExtent.width; SH = caps.currentExtent.height; }
   printf("COMP screen %ux%u (%s)\n", SW, SH, has_display ? "the display" : "headless");
   VkSwapchainCreateInfoKHR sci = { .sType = VK_STRUCTURE_TYPE_SWAPCHAIN_CREATE_INFO_KHR, .surface = surface, .minImageCount = caps.minImageCount < 3 ? 3 : caps.minImageCount,
      .imageFormat = VK_FORMAT_B8G8R8A8_UNORM, .imageColorSpace = VK_COLOR_SPACE_SRGB_NONLINEAR_KHR, .imageExtent = { SW, SH }, .imageArrayLayers = 1,
      .imageUsage = VK_IMAGE_USAGE_COLOR_ATTACHMENT_BIT, .imageSharingMode = VK_SHARING_MODE_EXCLUSIVE, .preTransform = VK_SURFACE_TRANSFORM_IDENTITY_BIT_KHR,
      .compositeAlpha = VK_COMPOSITE_ALPHA_OPAQUE_BIT_KHR, .presentMode = VK_PRESENT_MODE_FIFO_KHR, .clipped = VK_TRUE };
   VKOK(vkCreateSwapchainKHR(device, &sci, NULL, &swapchain));
   sc_count = MAX_SC_IMAGES;
   VkResult sr = vkGetSwapchainImagesKHR(device, swapchain, &sc_count, sc_images);
   if ((sr != VK_SUCCESS && sr != VK_INCOMPLETE) || sc_count == 0) { printf("COMP FAIL swapchain images (%d)\n", (int)sr); failures++; goto done; }
   VkSemaphoreCreateInfo semi = { .sType = VK_STRUCTURE_TYPE_SEMAPHORE_CREATE_INFO };
   VKOK(vkCreateSemaphore(device, &semi, NULL, &acquire_sem));
   VkImageViewCreateInfo ivci = { .sType = VK_STRUCTURE_TYPE_IMAGE_VIEW_CREATE_INFO, .viewType = VK_IMAGE_VIEW_TYPE_2D, .format = VK_FORMAT_B8G8R8A8_UNORM,
      .subresourceRange = { VK_IMAGE_ASPECT_COLOR_BIT, 0, 1, 0, 1 } };
   for (uint32_t i = 0; i < sc_count; i++) {
      ivci.image = sc_images[i];
      VKOK(vkCreateImageView(device, &ivci, NULL, &sc_views[i]));
      VKOK(vkCreateSemaphore(device, &semi, NULL, &render_sem[i]));
   }
   {
      int rc = comp_init(&comp, device, vkGetDeviceProcAddr, &mp, (uint32_t)family, VK_FORMAT_B8G8R8A8_UNORM);
      if (rc) { printf("COMP FAIL the renderer did not start (step %d)\n", rc); failures++; goto done; }
      comp_ready = 1;
   }

   /* the window manager, the socket, the input */
   gui = gui_new((int32_t)SW, (int32_t)SH);
   gui_enable_gpu_buffers(gui);
   lfd = socket(AF_UNIX, SOCK_STREAM | SOCK_NONBLOCK, 0);
   struct sockaddr_un addr;
   memset(&addr, 0, sizeof(addr));
   addr.sun_family = AF_UNIX;
   strncpy(addr.sun_path, socket_path, sizeof(addr.sun_path) - 1);
   unlink(socket_path);
   if (lfd < 0 || bind(lfd, (struct sockaddr *)&addr, sizeof(addr)) < 0 || listen(lfd, 8) < 0) { printf("COMP FAIL cannot listen on %s (errno %d)\n", socket_path, errno); failures++; goto done; }
   if (!no_input) {
      kbd = open("/dev/input/event0", O_RDONLY | O_NONBLOCK);
      mouse = open("/dev/input/event1", O_RDONLY | O_NONBLOCK);
      if (kbd < 0 || mouse < 0) { printf("COMP FAIL cannot open the input devices (%d %d)\n", kbd, mouse); failures++; goto done; }
      ioctl(kbd, EVIOCGRAB, 1);
      read_input(kbd, on_kbd);       /* what the rings hold from before is not ours */
      read_input(mouse, on_mouse);
      mdx = mdy = 0;
   }
   printf("COMP listening on %s\n", socket_path);
   if (argc > 1) {
      for (int i = 1; i < argc; i++) {
         pid_t p = start_program(argv[i], socket_path);
         printf("COMP %s %s (pid %d)\n", p > 0 ? "started" : "cannot start", argv[i], (int)p);
      }
   } else if (access("/bin/panel", X_OK) == 0 || access("/mnt/bin/panel", X_OK) == 0) {
      start_program("panel", socket_path);
   }

   const uint64_t t_start = now_ms();
   uint64_t last_stat = t_start;
   while (!quit_flag && !gui_quit_requested(gui)) {
      if (seconds && now_ms() - t_start >= (uint64_t)seconds * 1000) break;
      const int busy = gui_has_damage(gui) || gui_has_frame_callbacks(gui);
      struct pollfd pf[3 + MAX_CLIENTS];
      uint32_t ids[3 + MAX_CLIENTS];
      int np = 0;
      pf[np] = (struct pollfd){ .fd = lfd, .events = POLLIN }; ids[np++] = 0;
      if (kbd >= 0) { pf[np] = (struct pollfd){ .fd = kbd, .events = POLLIN }; ids[np++] = 0xfffffff1u; }
      if (mouse >= 0) { pf[np] = (struct pollfd){ .fd = mouse, .events = POLLIN }; ids[np++] = 0xfffffff2u; }
      for (uint32_t c = 1; c <= MAX_CLIENTS; c++)
         if (client_live[c]) { pf[np] = (struct pollfd){ .fd = client_fd[c], .events = POLLIN }; ids[np++] = c; }
      poll(pf, (nfds_t)np, busy ? 0 : 200);
      gui_set_time(gui, (uint32_t)now_ms());
      while (waitpid(-1, NULL, WNOHANG) > 0) {}   /* what we started and has exited */
      for (int i = 0; i < np; i++) {
         if (!(pf[i].revents & (POLLIN | POLLHUP))) continue;
         if (ids[i] == 0) {
            for (;;) {
               int fd = accept4(lfd, NULL, NULL, SOCK_NONBLOCK);
               if (fd < 0) break;
               uint32_t id = gui_add_client(gui);
               if (id > MAX_CLIENTS) { close(fd); gui_remove_client(gui, id); continue; }
               client_fd[id] = fd;
               client_live[id] = 1;
               clients_seen++;
               printf("COMP client %u connected\n", id);
            }
         } else if (ids[i] == 0xfffffff1u) {
            read_input(kbd, on_kbd);
         } else if (ids[i] == 0xfffffff2u) {
            read_input(mouse, on_mouse);
         } else {
            client_readable(ids[i]);
         }
      }
      flush_outputs();

      if (gui_has_damage(gui) || gui_has_frame_callbacks(gui)) {
         uint32_t idx = 0;
         VkResult ar = vkAcquireNextImageKHR(device, swapchain, 1000000000ull, acquire_sem, VK_NULL_HANDLE, &idx);
         if (ar != VK_SUCCESS && ar != VK_SUBOPTIMAL_KHR) { printf("COMP FAIL acquire (%d) at frame %llu\n", (int)ar, (unsigned long long)frames); failures++; break; }
         if (comp_frame(&comp, gui, sc_images[idx], sc_views[idx], SW, SH, acquire_sem, render_sem[idx], VK_IMAGE_LAYOUT_PRESENT_SRC_KHR) != 0) { printf("COMP FAIL frame %llu\n", (unsigned long long)frames); failures++; break; }
         VkPresentInfoKHR pinfo = { .sType = VK_STRUCTURE_TYPE_PRESENT_INFO_KHR, .waitSemaphoreCount = 1, .pWaitSemaphores = &render_sem[idx],
            .swapchainCount = 1, .pSwapchains = &swapchain, .pImageIndices = &idx };
         VkResult pr = vkQueuePresentKHR(queue, &pinfo);
         if (pr != VK_SUCCESS && pr != VK_SUBOPTIMAL_KHR) { printf("COMP FAIL present (%d) at frame %llu\n", (int)pr, (unsigned long long)frames); failures++; break; }
         frames++;
         gui_frame_done(gui, (uint32_t)now_ms());
         flush_outputs();
      }
      if (now_ms() - last_stat >= 5000) {
         last_stat = now_ms();
         printf("COMP %llu frames, %u draws in the last, %u imports, %u uploads, %u clients seen\n", (unsigned long long)frames, comp.draws, comp.imports, comp.uploads, clients_seen);
      }
   }
   printf("COMP quit after %llu frames (up to %u draws), %u imports, %u drops, %u uploads, %u clients seen\n", (unsigned long long)frames, comp.draws_max, comp.imports, comp.drops, comp.uploads, clients_seen);

done:
   if (comp_ready) comp_destroy(&comp);
   for (uint32_t c = 1; c <= MAX_CLIENTS; c++) if (client_live[c]) close(client_fd[c]);
   if (lfd >= 0) { close(lfd); unlink(socket_path); }
   if (kbd >= 0) close(kbd);
   if (mouse >= 0) close(mouse);
   if (device) {
      for (uint32_t i = 0; i < sc_count; i++) {
         if (sc_views[i]) vkDestroyImageView(device, sc_views[i], NULL);
         if (render_sem[i]) vkDestroySemaphore(device, render_sem[i], NULL);
      }
      if (acquire_sem) vkDestroySemaphore(device, acquire_sem, NULL);
      if (swapchain) vkDestroySwapchainKHR(device, swapchain, NULL);
   }
   if (surface) vkDestroySurfaceKHR(instance, surface, NULL);
   if (device) vkDestroyDevice(device, NULL);
   if (instance) vkDestroyInstance(instance, NULL);
   if (failures) { printf("COMP FAILED (%d)\n", failures); return 1; }
   printf("COMP DONE\n");
   return 0;
}

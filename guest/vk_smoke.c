// Minimal Vulkan round-trip, meant to run inside the guest.
//
// vulkaninfo only enumerates; this actually creates a device, allocates
// host-visible memory, records and submits a command, waits on a fence and
// reads the result back. On macOS that exercises the whole venus path:
// guest Mesa -> virtio-gpu -> virglrenderer -> MoltenVK -> Metal, including
// the blob mapping that the hypervisor installs into the guest.
//
//   cc -O2 -o vk_smoke vk_smoke.c -lvulkan && ./vk_smoke

#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <vulkan/vulkan.h>

#define PATTERN 0xdeadbeefu
#define WORDS 4096
#define BYTES (WORDS * sizeof(uint32_t))

#define CHECK(expr)                                                            \
    do {                                                                       \
        VkResult _r = (expr);                                                  \
        if (_r != VK_SUCCESS) {                                                \
            fprintf(stderr, "%s failed: %d\n", #expr, _r);                     \
            return 1;                                                          \
        }                                                                      \
    } while (0)

int main(void)
{
    // Ask for whatever the loader offers, so the device version below is the
    // real ceiling rather than an artefact of what this program requested.
    uint32_t instance_version = VK_API_VERSION_1_0;
    if (vkEnumerateInstanceVersion(&instance_version) != VK_SUCCESS)
        instance_version = VK_API_VERSION_1_0;
    printf("instance      : %u.%u.%u\n", VK_VERSION_MAJOR(instance_version),
           VK_VERSION_MINOR(instance_version), VK_VERSION_PATCH(instance_version));

    VkApplicationInfo app = {
        .sType = VK_STRUCTURE_TYPE_APPLICATION_INFO,
        .pApplicationName = "vk_smoke",
        .apiVersion = instance_version,
    };
    VkInstanceCreateInfo ici = {
        .sType = VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO,
        .pApplicationInfo = &app,
    };

    VkInstance instance;
    CHECK(vkCreateInstance(&ici, NULL, &instance));

    uint32_t n = 0;
    CHECK(vkEnumeratePhysicalDevices(instance, &n, NULL));
    if (n == 0) {
        fprintf(stderr, "no physical devices\n");
        return 1;
    }
    VkPhysicalDevice *devices = calloc(n, sizeof(*devices));
    CHECK(vkEnumeratePhysicalDevices(instance, &n, devices));
    VkPhysicalDevice phys = devices[0];

    VkPhysicalDeviceProperties props;
    vkGetPhysicalDeviceProperties(phys, &props);
    printf("device        : %s\n", props.deviceName);
    printf("device api    : %u.%u.%u\n", VK_VERSION_MAJOR(props.apiVersion),
           VK_VERSION_MINOR(props.apiVersion), VK_VERSION_PATCH(props.apiVersion));

    // Any queue that can run a transfer command will do.
    uint32_t qn = 0;
    vkGetPhysicalDeviceQueueFamilyProperties(phys, &qn, NULL);
    VkQueueFamilyProperties *qfs = calloc(qn, sizeof(*qfs));
    vkGetPhysicalDeviceQueueFamilyProperties(phys, &qn, qfs);

    uint32_t family = UINT32_MAX;
    for (uint32_t i = 0; i < qn; i++) {
        if (qfs[i].queueFlags & (VK_QUEUE_GRAPHICS_BIT | VK_QUEUE_COMPUTE_BIT |
                                 VK_QUEUE_TRANSFER_BIT)) {
            family = i;
            break;
        }
    }
    if (family == UINT32_MAX) {
        fprintf(stderr, "no usable queue family\n");
        return 1;
    }
    printf("queue family  : %u\n", family);

    float priority = 1.0f;
    VkDeviceQueueCreateInfo qci = {
        .sType = VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO,
        .queueFamilyIndex = family,
        .queueCount = 1,
        .pQueuePriorities = &priority,
    };
    VkDeviceCreateInfo dci = {
        .sType = VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO,
        .queueCreateInfoCount = 1,
        .pQueueCreateInfos = &qci,
    };

    VkDevice device;
    CHECK(vkCreateDevice(phys, &dci, NULL, &device));

    VkQueue queue;
    vkGetDeviceQueue(device, family, 0, &queue);

    VkBufferCreateInfo bci = {
        .sType = VK_STRUCTURE_TYPE_BUFFER_CREATE_INFO,
        .size = BYTES,
        .usage = VK_BUFFER_USAGE_TRANSFER_DST_BIT,
        .sharingMode = VK_SHARING_MODE_EXCLUSIVE,
    };
    VkBuffer buffer;
    CHECK(vkCreateBuffer(device, &bci, NULL, &buffer));

    VkMemoryRequirements req;
    vkGetBufferMemoryRequirements(device, buffer, &req);

    VkPhysicalDeviceMemoryProperties mem_props;
    vkGetPhysicalDeviceMemoryProperties(phys, &mem_props);

    const VkMemoryPropertyFlags want =
        VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT | VK_MEMORY_PROPERTY_HOST_COHERENT_BIT;
    uint32_t type = UINT32_MAX;
    for (uint32_t i = 0; i < mem_props.memoryTypeCount; i++) {
        if ((req.memoryTypeBits & (1u << i)) &&
            (mem_props.memoryTypes[i].propertyFlags & want) == want) {
            type = i;
            break;
        }
    }
    if (type == UINT32_MAX) {
        fprintf(stderr, "no host-visible coherent memory type\n");
        return 1;
    }
    printf("memory type   : %u (%llu bytes)\n", type, (unsigned long long)req.size);

    VkMemoryAllocateInfo mai = {
        .sType = VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO,
        .allocationSize = req.size,
        .memoryTypeIndex = type,
    };
    VkDeviceMemory memory;
    CHECK(vkAllocateMemory(device, &mai, NULL, &memory));
    CHECK(vkBindBufferMemory(device, buffer, memory, 0));

    // Poison from the host side so a no-op submit cannot pass.
    void *mapped = NULL;
    CHECK(vkMapMemory(device, memory, 0, BYTES, 0, &mapped));
    memset(mapped, 0, BYTES);
    vkUnmapMemory(device, memory);

    VkCommandPoolCreateInfo pci = {
        .sType = VK_STRUCTURE_TYPE_COMMAND_POOL_CREATE_INFO,
        .queueFamilyIndex = family,
    };
    VkCommandPool pool;
    CHECK(vkCreateCommandPool(device, &pci, NULL, &pool));

    VkCommandBufferAllocateInfo cbai = {
        .sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_ALLOCATE_INFO,
        .commandPool = pool,
        .level = VK_COMMAND_BUFFER_LEVEL_PRIMARY,
        .commandBufferCount = 1,
    };
    VkCommandBuffer cmd;
    CHECK(vkAllocateCommandBuffers(device, &cbai, &cmd));

    VkCommandBufferBeginInfo cbbi = {
        .sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO,
        .flags = VK_COMMAND_BUFFER_USAGE_ONE_TIME_SUBMIT_BIT,
    };
    CHECK(vkBeginCommandBuffer(cmd, &cbbi));
    vkCmdFillBuffer(cmd, buffer, 0, BYTES, PATTERN);

    VkMemoryBarrier barrier = {
        .sType = VK_STRUCTURE_TYPE_MEMORY_BARRIER,
        .srcAccessMask = VK_ACCESS_TRANSFER_WRITE_BIT,
        .dstAccessMask = VK_ACCESS_HOST_READ_BIT,
    };
    vkCmdPipelineBarrier(cmd, VK_PIPELINE_STAGE_TRANSFER_BIT,
                         VK_PIPELINE_STAGE_HOST_BIT, 0, 1, &barrier, 0, NULL, 0, NULL);
    CHECK(vkEndCommandBuffer(cmd));

    VkFenceCreateInfo fci = {.sType = VK_STRUCTURE_TYPE_FENCE_CREATE_INFO};
    VkFence fence;
    CHECK(vkCreateFence(device, &fci, NULL, &fence));

    VkSubmitInfo si = {
        .sType = VK_STRUCTURE_TYPE_SUBMIT_INFO,
        .commandBufferCount = 1,
        .pCommandBuffers = &cmd,
    };
    CHECK(vkQueueSubmit(queue, 1, &si, fence));

    VkResult wait = vkWaitForFences(device, 1, &fence, VK_TRUE, 5ull * 1000 * 1000 * 1000);
    if (wait != VK_SUCCESS) {
        fprintf(stderr, "fence wait: %d\n", wait);
        return 1;
    }

    CHECK(vkMapMemory(device, memory, 0, BYTES, 0, &mapped));
    const uint32_t *out = mapped;
    size_t bad = 0;
    for (size_t i = 0; i < WORDS; i++) {
        if (out[i] != PATTERN) {
            if (bad == 0)
                fprintf(stderr, "word %zu = %08x, want %08x\n", i, out[i], PATTERN);
            bad++;
        }
    }
    vkUnmapMemory(device, memory);

    printf("fill %u words : %s\n", WORDS, bad == 0 ? "OK" : "MISMATCH");

    vkDestroyFence(device, fence, NULL);
    vkDestroyCommandPool(device, pool, NULL);
    vkDestroyBuffer(device, buffer, NULL);
    vkFreeMemory(device, memory, NULL);
    vkDestroyDevice(device, NULL);
    vkDestroyInstance(instance, NULL);
    free(qfs);
    free(devices);

    return bad == 0 ? 0 : 1;
}

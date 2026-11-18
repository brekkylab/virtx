/*
 * What the machine tests run on a guest's GPU, built in the guest against its Vulkan loader.
 * It uses the first venus device that stands for hardware, as the tests' own check counts one.
 *
 *   gpu compute <triple.spv>   fills a buffer, dispatches triple.comp over it, reads it back
 *                              and checks every word; prints `computed <n> on <device>`
 *   gpu heaps                  prints `heap <i> size=<bytes>` for each of the device's heaps
 *   gpu maps <MiB> <count>     allocates and maps <count> host-visible buffers of <MiB> each,
 *                              holding every mapping; prints `map <i> alloc=<r> map=<r>` for
 *                              each until one fails
 *
 * Exits 0 when it ran to the end, whatever `maps` found; anything else is a failure to say.
 */
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <vulkan/vulkan.h>

#define CHECK(call)                                                       \
    do {                                                                  \
        VkResult r_ = (call);                                             \
        if (r_ != VK_SUCCESS) {                                           \
            fprintf(stderr, "%s failed: %d\n", #call, (int)r_);           \
            exit(2);                                                      \
        }                                                                 \
    } while (0)

static const char *const SOFTWARE[] = {"llvmpipe", "lavapipe", "SwiftShader"};

static VkInstance instance;
static VkPhysicalDevice physical;
static VkPhysicalDeviceProperties properties;
static VkDevice device;
static uint32_t family;

static void open_gpu(void) {
    VkApplicationInfo app = {
        .sType = VK_STRUCTURE_TYPE_APPLICATION_INFO,
        .pApplicationName = "virtx-gpu-test",
        .apiVersion = VK_API_VERSION_1_1,
    };
    VkInstanceCreateInfo info = {
        .sType = VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO,
        .pApplicationInfo = &app,
    };
    CHECK(vkCreateInstance(&info, NULL, &instance));

    VkPhysicalDevice devices[16];
    uint32_t count = 16;
    CHECK(vkEnumeratePhysicalDevices(instance, &count, devices));
    for (uint32_t i = 0; i < count && !physical; i++) {
        VkPhysicalDeviceProperties p;
        vkGetPhysicalDeviceProperties(devices[i], &p);
        int software = 0;
        for (size_t s = 0; s < sizeof SOFTWARE / sizeof *SOFTWARE; s++) {
            software |= strstr(p.deviceName, SOFTWARE[s]) != NULL;
        }
        if (strstr(p.deviceName, "Venus") && !software) {
            physical = devices[i];
            properties = p;
        }
    }
    if (!physical) {
        fprintf(stderr, "no venus device with hardware behind it among %u\n", count);
        exit(3);
    }

    VkQueueFamilyProperties families[16];
    uint32_t families_count = 16;
    vkGetPhysicalDeviceQueueFamilyProperties(physical, &families_count, families);
    family = UINT32_MAX;
    for (uint32_t i = 0; i < families_count && family == UINT32_MAX; i++) {
        if (families[i].queueFlags & VK_QUEUE_COMPUTE_BIT) family = i;
    }
    if (family == UINT32_MAX) {
        fprintf(stderr, "no compute queue on %s\n", properties.deviceName);
        exit(3);
    }

    float priority = 1.0f;
    VkDeviceQueueCreateInfo queue = {
        .sType = VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO,
        .queueFamilyIndex = family,
        .queueCount = 1,
        .pQueuePriorities = &priority,
    };
    VkDeviceCreateInfo device_info = {
        .sType = VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO,
        .queueCreateInfoCount = 1,
        .pQueueCreateInfos = &queue,
    };
    CHECK(vkCreateDevice(physical, &device_info, NULL, &device));
}

/* A memory type `bits` allows that is host-visible and coherent, which venus maps through
 * the window. */
static uint32_t host_visible(uint32_t bits) {
    VkPhysicalDeviceMemoryProperties memory;
    vkGetPhysicalDeviceMemoryProperties(physical, &memory);
    VkMemoryPropertyFlags want =
        VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT | VK_MEMORY_PROPERTY_HOST_COHERENT_BIT;
    for (uint32_t i = 0; i < memory.memoryTypeCount; i++) {
        if ((bits & (1u << i)) && (memory.memoryTypes[i].propertyFlags & want) == want) return i;
    }
    fprintf(stderr, "no host-visible coherent memory type in %#x\n", bits);
    exit(3);
}

/* A storage buffer of `size` bytes, its memory allocated (the result in *alloc) and, if that
 * worked, mapped (the result in *map, the pointer in *mapped). */
static void buffer(VkDeviceSize size, VkBuffer *out, VkDeviceMemory *memory, void **mapped,
                   VkResult *alloc, VkResult *map) {
    VkBufferCreateInfo info = {
        .sType = VK_STRUCTURE_TYPE_BUFFER_CREATE_INFO,
        .size = size,
        .usage = VK_BUFFER_USAGE_STORAGE_BUFFER_BIT,
        .sharingMode = VK_SHARING_MODE_EXCLUSIVE,
    };
    CHECK(vkCreateBuffer(device, &info, NULL, out));
    VkMemoryRequirements needs;
    vkGetBufferMemoryRequirements(device, *out, &needs);
    VkMemoryAllocateInfo allocate = {
        .sType = VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO,
        .allocationSize = needs.size,
        .memoryTypeIndex = host_visible(needs.memoryTypeBits),
    };
    *map = VK_NOT_READY;
    *alloc = vkAllocateMemory(device, &allocate, NULL, memory);
    if (*alloc != VK_SUCCESS) return;
    CHECK(vkBindBufferMemory(device, *out, *memory, 0));
    *map = vkMapMemory(device, *memory, 0, VK_WHOLE_SIZE, 0, mapped);
}

static int compute(const char *spv_path) {
    FILE *f = fopen(spv_path, "rb");
    if (!f) {
        perror(spv_path);
        return 2;
    }
    fseek(f, 0, SEEK_END);
    long spv_size = ftell(f);
    fseek(f, 0, SEEK_SET);
    uint32_t *spv = malloc(spv_size);
    if (fread(spv, 1, spv_size, f) != (size_t)spv_size) {
        perror(spv_path);
        return 2;
    }
    fclose(f);

    const uint32_t n = 1u << 20;
    VkBuffer data;
    VkDeviceMemory memory;
    uint32_t *words;
    VkResult alloc, map;
    buffer((VkDeviceSize)n * sizeof *words, &data, &memory, (void **)&words, &alloc, &map);
    CHECK(alloc);
    CHECK(map);
    for (uint32_t i = 0; i < n; i++) words[i] = i;

    VkShaderModuleCreateInfo module_info = {
        .sType = VK_STRUCTURE_TYPE_SHADER_MODULE_CREATE_INFO,
        .codeSize = (size_t)spv_size,
        .pCode = spv,
    };
    VkShaderModule module;
    CHECK(vkCreateShaderModule(device, &module_info, NULL, &module));

    VkDescriptorSetLayoutBinding binding = {
        .binding = 0,
        .descriptorType = VK_DESCRIPTOR_TYPE_STORAGE_BUFFER,
        .descriptorCount = 1,
        .stageFlags = VK_SHADER_STAGE_COMPUTE_BIT,
    };
    VkDescriptorSetLayoutCreateInfo set_layout_info = {
        .sType = VK_STRUCTURE_TYPE_DESCRIPTOR_SET_LAYOUT_CREATE_INFO,
        .bindingCount = 1,
        .pBindings = &binding,
    };
    VkDescriptorSetLayout set_layout;
    CHECK(vkCreateDescriptorSetLayout(device, &set_layout_info, NULL, &set_layout));
    VkPipelineLayoutCreateInfo layout_info = {
        .sType = VK_STRUCTURE_TYPE_PIPELINE_LAYOUT_CREATE_INFO,
        .setLayoutCount = 1,
        .pSetLayouts = &set_layout,
    };
    VkPipelineLayout layout;
    CHECK(vkCreatePipelineLayout(device, &layout_info, NULL, &layout));
    VkComputePipelineCreateInfo pipeline_info = {
        .sType = VK_STRUCTURE_TYPE_COMPUTE_PIPELINE_CREATE_INFO,
        .stage =
            {
                .sType = VK_STRUCTURE_TYPE_PIPELINE_SHADER_STAGE_CREATE_INFO,
                .stage = VK_SHADER_STAGE_COMPUTE_BIT,
                .module = module,
                .pName = "main",
            },
        .layout = layout,
    };
    VkPipeline pipeline;
    CHECK(vkCreateComputePipelines(device, VK_NULL_HANDLE, 1, &pipeline_info, NULL, &pipeline));

    VkDescriptorPoolSize pool_size = {VK_DESCRIPTOR_TYPE_STORAGE_BUFFER, 1};
    VkDescriptorPoolCreateInfo pool_info = {
        .sType = VK_STRUCTURE_TYPE_DESCRIPTOR_POOL_CREATE_INFO,
        .maxSets = 1,
        .poolSizeCount = 1,
        .pPoolSizes = &pool_size,
    };
    VkDescriptorPool pool;
    CHECK(vkCreateDescriptorPool(device, &pool_info, NULL, &pool));
    VkDescriptorSetAllocateInfo set_info = {
        .sType = VK_STRUCTURE_TYPE_DESCRIPTOR_SET_ALLOCATE_INFO,
        .descriptorPool = pool,
        .descriptorSetCount = 1,
        .pSetLayouts = &set_layout,
    };
    VkDescriptorSet set;
    CHECK(vkAllocateDescriptorSets(device, &set_info, &set));
    VkDescriptorBufferInfo described = {data, 0, VK_WHOLE_SIZE};
    VkWriteDescriptorSet write = {
        .sType = VK_STRUCTURE_TYPE_WRITE_DESCRIPTOR_SET,
        .dstSet = set,
        .dstBinding = 0,
        .descriptorCount = 1,
        .descriptorType = VK_DESCRIPTOR_TYPE_STORAGE_BUFFER,
        .pBufferInfo = &described,
    };
    vkUpdateDescriptorSets(device, 1, &write, 0, NULL);

    VkCommandPoolCreateInfo command_pool_info = {
        .sType = VK_STRUCTURE_TYPE_COMMAND_POOL_CREATE_INFO,
        .queueFamilyIndex = family,
    };
    VkCommandPool command_pool;
    CHECK(vkCreateCommandPool(device, &command_pool_info, NULL, &command_pool));
    VkCommandBufferAllocateInfo command_info = {
        .sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_ALLOCATE_INFO,
        .commandPool = command_pool,
        .level = VK_COMMAND_BUFFER_LEVEL_PRIMARY,
        .commandBufferCount = 1,
    };
    VkCommandBuffer commands;
    CHECK(vkAllocateCommandBuffers(device, &command_info, &commands));
    VkCommandBufferBeginInfo begin = {
        .sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO,
        .flags = VK_COMMAND_BUFFER_USAGE_ONE_TIME_SUBMIT_BIT,
    };
    CHECK(vkBeginCommandBuffer(commands, &begin));
    vkCmdBindPipeline(commands, VK_PIPELINE_BIND_POINT_COMPUTE, pipeline);
    vkCmdBindDescriptorSets(commands, VK_PIPELINE_BIND_POINT_COMPUTE, layout, 0, 1, &set, 0,
                            NULL);
    vkCmdDispatch(commands, n / 256, 1, 1);
    /* The shader's writes, made visible to the host's reads of the mapping. */
    VkMemoryBarrier barrier = {
        .sType = VK_STRUCTURE_TYPE_MEMORY_BARRIER,
        .srcAccessMask = VK_ACCESS_SHADER_WRITE_BIT,
        .dstAccessMask = VK_ACCESS_HOST_READ_BIT,
    };
    vkCmdPipelineBarrier(commands, VK_PIPELINE_STAGE_COMPUTE_SHADER_BIT,
                         VK_PIPELINE_STAGE_HOST_BIT, 0, 1, &barrier, 0, NULL, 0, NULL);
    CHECK(vkEndCommandBuffer(commands));

    VkFenceCreateInfo fence_info = {.sType = VK_STRUCTURE_TYPE_FENCE_CREATE_INFO};
    VkFence fence;
    CHECK(vkCreateFence(device, &fence_info, NULL, &fence));
    VkQueue queue;
    vkGetDeviceQueue(device, family, 0, &queue);
    VkSubmitInfo submit = {
        .sType = VK_STRUCTURE_TYPE_SUBMIT_INFO,
        .commandBufferCount = 1,
        .pCommandBuffers = &commands,
    };
    CHECK(vkQueueSubmit(queue, 1, &submit, fence));
    CHECK(vkWaitForFences(device, 1, &fence, VK_TRUE, UINT64_C(30000000000)));

    for (uint32_t i = 0; i < n; i++) {
        if (words[i] != i * 3u + 1u) {
            fprintf(stderr, "word %u is %u, not %u\n", i, words[i], i * 3u + 1u);
            return 4;
        }
    }
    printf("computed %u on %s\n", n, properties.deviceName);
    return 0;
}

static int heaps(void) {
    VkPhysicalDeviceMemoryProperties memory;
    vkGetPhysicalDeviceMemoryProperties(physical, &memory);
    for (uint32_t i = 0; i < memory.memoryHeapCount; i++) {
        printf("heap %u size=%llu\n", i, (unsigned long long)memory.memoryHeaps[i].size);
    }
    return 0;
}

static int maps(unsigned mib, unsigned count) {
    for (unsigned i = 0; i < count; i++) {
        VkBuffer b;
        VkDeviceMemory memory;
        void *mapped = NULL;
        VkResult alloc, map;
        buffer((VkDeviceSize)mib << 20, &b, &memory, &mapped, &alloc, &map);
        printf("map %u alloc=%d map=%d\n", i, (int)alloc, (int)map);
        if (alloc != VK_SUCCESS || map != VK_SUCCESS) break;
        /* Touched, so a mapping that only looks made is found out here. */
        memset(mapped, 0xa5, (size_t)mib << 20);
    }
    return 0;
}

int main(int argc, char **argv) {
    if (argc == 3 && !strcmp(argv[1], "compute")) {
        open_gpu();
        return compute(argv[2]);
    }
    if (argc == 2 && !strcmp(argv[1], "heaps")) {
        open_gpu();
        return heaps();
    }
    if (argc == 4 && !strcmp(argv[1], "maps")) {
        open_gpu();
        return maps((unsigned)atoi(argv[2]), (unsigned)atoi(argv[3]));
    }
    fprintf(stderr, "usage: gpu compute <spv> | gpu heaps | gpu maps <MiB> <count>\n");
    return 1;
}

// Classify an image with SqueezeNet v1.1 through ncnn's Vulkan backend, and
// check the result against ncnn's CPU backend.
//
// The point is not the model -- it is that a real convolutional network runs
// on the guest GPU over venus and comes back with the same answer the CPU
// gives. Built and run by scripts/run-ncnn-squeezenet.sh.
//
// ncnn must be configured with -DNCNN_VULKAN=ON -DNCNN_SIMPLEOCV=ON; the
// latter is what provides cv::imread here, so no OpenCV is needed in the
// guest.

#include "benchmark.h"
#include "gpu.h"
#include "net.h"
#include "simpleocv.h"

#include <algorithm>
#include <cmath>
#include <cstdio>
#include <fstream>
#include <string>
#include <vector>

namespace {

// SqueezeNet v1.1 was trained on 227x227 BGR pixels with the per-channel
// ImageNet mean subtracted and no scaling.
const int kInputSize = 227;
const float kMeanVals[3] = {104.f, 117.f, 123.f};

const int kWarmup = 1;
const int kIters = 5;
const int kTopK = 5;

std::vector<std::string> load_labels(const std::string& path)
{
    std::vector<std::string> labels;
    std::ifstream in(path.c_str());
    std::string line;
    while (std::getline(in, line))
    {
        if (!line.empty() && line[line.size() - 1] == '\r')
            line.erase(line.size() - 1);
        labels.push_back(line);
    }
    return labels;
}

// Returns the mean wall time per iteration, or -1.0 on failure.
double classify(const std::string& assets, const cv::Mat& bgr, bool use_vulkan,
                std::vector<float>& scores)
{
    ncnn::Net net;
    net.opt.use_vulkan_compute = use_vulkan;

    if (net.load_param((assets + "/squeezenet_v1.1.param").c_str()))
        return -1.0;
    if (net.load_model((assets + "/squeezenet_v1.1.bin").c_str()))
        return -1.0;

    ncnn::Mat in = ncnn::Mat::from_pixels_resize(bgr.data, ncnn::Mat::PIXEL_BGR,
                                                 bgr.cols, bgr.rows,
                                                 kInputSize, kInputSize);
    in.substract_mean_normalize(kMeanVals, 0);

    ncnn::Mat out;
    double elapsed = 0.0;
    for (int i = 0; i < kWarmup + kIters; i++)
    {
        const double start = ncnn::get_current_time();

        ncnn::Extractor ex = net.create_extractor();
        if (ex.input("data", in))
            return -1.0;
        if (ex.extract("prob", out))
            return -1.0;

        if (i >= kWarmup)
            elapsed += ncnn::get_current_time() - start;
    }

    scores.assign(out.w, 0.f);
    for (int i = 0; i < out.w; i++)
        scores[i] = out[i];

    return elapsed / kIters;
}

std::vector<std::pair<float, int> > top_k(const std::vector<float>& scores, int k)
{
    std::vector<std::pair<float, int> > ranked(scores.size());
    for (size_t i = 0; i < scores.size(); i++)
        ranked[i] = std::make_pair(scores[i], (int)i);

    k = std::min<int>(k, (int)ranked.size());
    std::partial_sort(ranked.begin(), ranked.begin() + k, ranked.end(),
                      std::greater<std::pair<float, int> >());
    ranked.resize(k);
    return ranked;
}

const char* label_for(const std::vector<std::string>& labels, int index)
{
    return index >= 0 && index < (int)labels.size() ? labels[index].c_str() : "?";
}

} // namespace

int main(int argc, char** argv)
{
    if (argc != 3)
    {
        fprintf(stderr, "usage: %s <assets-dir> <image>\n", argv[0]);
        return 2;
    }

    // ncnn logs device probing to stderr while creating the Vulkan instance;
    // without this the two streams interleave mid-line once stdout is a pipe
    // rather than a tty. Unbuffered rather than _IOLBF because musl ignores a
    // zero size, and the output here is a dozen lines.
    setvbuf(stdout, NULL, _IONBF, 0);

    const std::string assets = argv[1];
    const std::string image_path = argv[2];

    cv::Mat bgr = cv::imread(image_path, 1);
    if (bgr.empty())
    {
        fprintf(stderr, "cannot read %s\n", image_path.c_str());
        return 1;
    }

    const std::vector<std::string> labels = load_labels(assets + "/synset_words.txt");

    if (ncnn::create_gpu_instance() != 0 || ncnn::get_gpu_count() == 0)
    {
        fprintf(stderr, "no vulkan device\n");
        return 1;
    }

    const ncnn::GpuInfo& gpu = ncnn::get_gpu_info();
    const uint32_t api = gpu.api_version();
    printf("device        : %s\n", gpu.device_name());
    printf("device api    : %u.%u.%u\n", VK_VERSION_MAJOR(api), VK_VERSION_MINOR(api),
           VK_VERSION_PATCH(api));
    printf("fp16 storage  : %s\n", gpu.support_fp16_storage() ? "yes" : "no");
    printf("fp16 math     : %s\n", gpu.support_fp16_arithmetic() ? "yes" : "no");
    printf("image         : %s (%dx%d)\n", image_path.c_str(), bgr.cols, bgr.rows);
    printf("model         : squeezenet_v1.1 (%d classes)\n", (int)labels.size());

    std::vector<float> cpu_scores;
    const double cpu_ms = classify(assets, bgr, false, cpu_scores);

    std::vector<float> gpu_scores;
    const double gpu_ms = classify(assets, bgr, true, gpu_scores);

    ncnn::destroy_gpu_instance();

    if (cpu_ms < 0.0 || gpu_ms < 0.0)
    {
        fprintf(stderr, "inference failed (cpu %.2f, gpu %.2f)\n", cpu_ms, gpu_ms);
        return 1;
    }

    printf("cpu           : %.1f ms/iter\n", cpu_ms);
    printf("vulkan        : %.1f ms/iter\n", gpu_ms);

    const std::vector<std::pair<float, int> > gpu_top = top_k(gpu_scores, kTopK);
    printf("\ntop-%d on vulkan:\n", (int)gpu_top.size());
    for (size_t i = 0; i < gpu_top.size(); i++)
        printf("  %6.2f%%  %s\n", gpu_top[i].first * 100.f,
               label_for(labels, gpu_top[i].second));
    printf("\n");

    if (cpu_scores.size() != gpu_scores.size())
    {
        fprintf(stderr, "output size mismatch: cpu %zu, vulkan %zu\n", cpu_scores.size(),
                gpu_scores.size());
        return 1;
    }

    float max_diff = 0.f;
    for (size_t i = 0; i < cpu_scores.size(); i++)
        max_diff = std::max(max_diff, std::fabs(cpu_scores[i] - gpu_scores[i]));

    const int cpu_top1 = top_k(cpu_scores, 1)[0].second;
    const int gpu_top1 = gpu_top[0].second;
    const bool agree = cpu_top1 == gpu_top1;

    printf("cpu/vulkan top-1 : %s\n", agree ? "match" : "MISMATCH");
    if (!agree)
        fprintf(stderr, "  cpu %s\n  gpu %s\n", label_for(labels, cpu_top1),
                label_for(labels, gpu_top1));
    printf("max score diff   : %.2e\n", max_diff);

    // fp16 on the GPU path against fp32 on the CPU path; a few 1e-3 of drift
    // on a softmax output is expected, a different answer is not.
    return agree ? 0 : 1;
}

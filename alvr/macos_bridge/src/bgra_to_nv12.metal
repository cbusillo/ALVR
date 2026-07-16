#include <metal_stdlib>

using namespace metal;

struct ConversionParams {
    uint source_eye_width;
    uint output_eye_width;
    uint source_height;
    uint output_height;
};

constexpr sampler bilinear_sampler(
    coord::pixel,
    address::clamp_to_edge,
    filter::linear);

static float scaled_center(uint output, uint source_extent, uint output_extent) {
    return (float(output) + 0.5f) * float(source_extent) / float(output_extent);
}

static float2 source_position(
    uint output_x,
    uint output_y,
    constant ConversionParams &params) {
    uint eye = output_x / params.output_eye_width;
    uint eye_x = output_x - eye * params.output_eye_width;
    float source_eye_x = clamp(
        scaled_center(eye_x, params.source_eye_width, params.output_eye_width),
        0.5f,
        float(params.source_eye_width) - 0.5f);
    float source_y = clamp(
        scaled_center(output_y, params.source_height, params.output_height),
        0.5f,
        float(params.source_height) - 0.5f);
    return float2(float(eye * params.source_eye_width) + source_eye_x, source_y);
}

static float3 sample_rgb(
    texture2d<float, access::sample> source,
    uint output_x,
    uint output_y,
    constant ConversionParams &params) {
    return source.sample(
        bilinear_sampler,
        source_position(output_x, output_y, params)).rgb;
}

static float luma(float3 rgb) {
    return (16.0f + 219.0f * dot(rgb, float3(0.2126f, 0.7152f, 0.0722f)))
        / 255.0f;
}

kernel void bgra_to_nv12(
    texture2d<float, access::sample> source [[texture(0)]],
    texture2d<float, access::write> destination_y [[texture(1)]],
    texture2d<float, access::write> destination_uv [[texture(2)]],
    constant ConversionParams &params [[buffer(0)]],
    uint2 chroma_position [[thread_position_in_grid]]) {
    uint output_width = params.output_eye_width * 2;
    uint2 output_origin = chroma_position * 2;
    if (output_origin.x >= output_width || output_origin.y >= params.output_height) {
        return;
    }

    float3 rgb_00 = sample_rgb(source, output_origin.x, output_origin.y, params);
    float3 rgb_10 = sample_rgb(source, output_origin.x + 1, output_origin.y, params);
    float3 rgb_01 = sample_rgb(source, output_origin.x, output_origin.y + 1, params);
    float3 rgb_11 = sample_rgb(source, output_origin.x + 1, output_origin.y + 1, params);

    destination_y.write(float4(luma(rgb_00), 0.0f, 0.0f, 1.0f), output_origin);
    destination_y.write(
        float4(luma(rgb_10), 0.0f, 0.0f, 1.0f), output_origin + uint2(1, 0));
    destination_y.write(
        float4(luma(rgb_01), 0.0f, 0.0f, 1.0f), output_origin + uint2(0, 1));
    destination_y.write(
        float4(luma(rgb_11), 0.0f, 0.0f, 1.0f), output_origin + uint2(1, 1));

    float3 rgb = (rgb_00 + rgb_10 + rgb_01 + rgb_11) * 0.25f;
    float y = dot(rgb, float3(0.2126f, 0.7152f, 0.0722f));
    float cb = (128.0f + 112.0f * (rgb.b - y) / (1.0f - 0.0722f)) / 255.0f;
    float cr = (128.0f + 112.0f * (rgb.r - y) / (1.0f - 0.2126f)) / 255.0f;
    destination_uv.write(float4(cb, cr, 0.0f, 1.0f), chroma_position);
}

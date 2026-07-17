#import <CoreVideo/CoreVideo.h>
#import <IOSurface/IOSurface.h>
#import <Metal/Metal.h>

#include <cstddef>
#include <cstdint>
#include <cstdio>

struct ConversionParams {
    uint32_t source_eye_width;
    uint32_t output_eye_width;
    uint32_t source_height;
    uint32_t output_height;
};

struct MetalConverter {
    id<MTLDevice> device;
    id<MTLCommandQueue> queue;
    id<MTLComputePipelineState> pipeline;
    CVMetalTextureCacheRef texture_cache;
};

static void set_error(char *buffer, size_t capacity, const char *message) {
    if (buffer != nullptr && capacity != 0) {
        std::snprintf(buffer, capacity, "%s", message);
    }
}

extern "C" void *alvr_metal_converter_create(
    const uint8_t *library_bytes,
    size_t library_size,
    char *error_buffer,
    size_t error_capacity) {
    @autoreleasepool {
        if (library_bytes == nullptr || library_size == 0) {
            set_error(error_buffer, error_capacity, "embedded Metal library is empty");
            return nullptr;
        }
        id<MTLDevice> device = MTLCreateSystemDefaultDevice();
        if (device == nil) {
            set_error(error_buffer, error_capacity, "MTLCreateSystemDefaultDevice failed");
            return nullptr;
        }
        dispatch_data_t data = dispatch_data_create(
            library_bytes,
            library_size,
            dispatch_get_global_queue(QOS_CLASS_USER_INITIATED, 0),
            DISPATCH_DATA_DESTRUCTOR_DEFAULT);
        NSError *error = nil;
        id<MTLLibrary> library = [device newLibraryWithData:data error:&error];
        if (library == nil) {
            set_error(
                error_buffer,
                error_capacity,
                error.localizedDescription.UTF8String ?: "Metal library loading failed");
            return nullptr;
        }
        id<MTLFunction> function = [library newFunctionWithName:@"bgra_to_nv12"];
        if (function == nil) {
            set_error(error_buffer, error_capacity, "bgra_to_nv12 function is missing");
            return nullptr;
        }
        id<MTLComputePipelineState> pipeline =
            [device newComputePipelineStateWithFunction:function error:&error];
        if (pipeline == nil) {
            set_error(
                error_buffer,
                error_capacity,
                error.localizedDescription.UTF8String ?: "Metal pipeline creation failed");
            return nullptr;
        }
        id<MTLCommandQueue> queue = [device newCommandQueue];
        if (queue == nil) {
            set_error(error_buffer, error_capacity, "Metal command queue creation failed");
            return nullptr;
        }
        CVMetalTextureCacheRef texture_cache = nullptr;
        CVReturn cache_status = CVMetalTextureCacheCreate(
            kCFAllocatorDefault, nullptr, device, nullptr, &texture_cache);
        if (cache_status != kCVReturnSuccess || texture_cache == nullptr) {
            set_error(error_buffer, error_capacity, "CVMetalTextureCacheCreate failed");
            return nullptr;
        }

        auto *converter = new MetalConverter{
            device,
            queue,
            pipeline,
            texture_cache,
        };
        return converter;
    }
}

extern "C" void alvr_metal_converter_destroy(void *opaque_converter) {
    auto *converter = static_cast<MetalConverter *>(opaque_converter);
    if (converter == nullptr) {
        return;
    }
    if (converter->texture_cache != nullptr) {
        CFRelease(converter->texture_cache);
    }
    delete converter;
}

extern "C" int alvr_metal_converter_convert(
    void *opaque_converter,
    IOSurfaceRef source_surface,
    CVPixelBufferRef destination_buffer,
    uint32_t source_width,
    uint32_t source_height,
    uint64_t *gpu_duration_ns,
    char *error_buffer,
    size_t error_capacity) {
    @autoreleasepool {
        if (gpu_duration_ns != nullptr) {
            *gpu_duration_ns = 0;
        }
        auto *converter = static_cast<MetalConverter *>(opaque_converter);
        if (converter == nullptr || source_surface == nullptr ||
            destination_buffer == nullptr || source_width == 0 || source_height == 0 ||
            source_width % 4 != 0 || source_height % 2 != 0) {
            set_error(error_buffer, error_capacity, "invalid Metal conversion arguments");
            return 1;
        }

        uint32_t output_width = static_cast<uint32_t>(
            CVPixelBufferGetWidth(destination_buffer));
        uint32_t output_height = static_cast<uint32_t>(
            CVPixelBufferGetHeight(destination_buffer));
        if (output_width == 0 || output_width % 4 != 0 ||
            output_height == 0 || output_height % 2 != 0 ||
            CVPixelBufferGetPlaneCount(destination_buffer) != 2) {
            set_error(error_buffer, error_capacity, "destination CVPixelBuffer shape mismatch");
            return 2;
        }

        MTLTextureDescriptor *source_descriptor =
            [MTLTextureDescriptor texture2DDescriptorWithPixelFormat:MTLPixelFormatBGRA8Unorm
                                                               width:source_width
                                                              height:source_height
                                                           mipmapped:NO];
        source_descriptor.storageMode = MTLStorageModeShared;
        source_descriptor.usage = MTLTextureUsageShaderRead;
        id<MTLTexture> source_texture =
            [converter->device newTextureWithDescriptor:source_descriptor
                                               iosurface:source_surface
                                                   plane:0];
        if (source_texture == nil) {
            set_error(error_buffer, error_capacity, "source IOSurface texture creation failed");
            return 3;
        }

        CVMetalTextureRef y_reference = nullptr;
        CVReturn y_status = CVMetalTextureCacheCreateTextureFromImage(
            kCFAllocatorDefault,
            converter->texture_cache,
            destination_buffer,
            nullptr,
            MTLPixelFormatR8Unorm,
            output_width,
            output_height,
            0,
            &y_reference);
        CVMetalTextureRef uv_reference = nullptr;
        CVReturn uv_status = CVMetalTextureCacheCreateTextureFromImage(
            kCFAllocatorDefault,
            converter->texture_cache,
            destination_buffer,
            nullptr,
            MTLPixelFormatRG8Unorm,
            output_width / 2,
            output_height / 2,
            1,
            &uv_reference);
        if (y_status != kCVReturnSuccess || uv_status != kCVReturnSuccess ||
            y_reference == nullptr || uv_reference == nullptr) {
            if (y_reference != nullptr) CFRelease(y_reference);
            if (uv_reference != nullptr) CFRelease(uv_reference);
            set_error(error_buffer, error_capacity, "destination Metal texture creation failed");
            return 4;
        }

        id<MTLTexture> y_texture = CVMetalTextureGetTexture(y_reference);
        id<MTLTexture> uv_texture = CVMetalTextureGetTexture(uv_reference);
        id<MTLCommandBuffer> command_buffer = [converter->queue commandBuffer];
        id<MTLComputeCommandEncoder> encoder = [command_buffer computeCommandEncoder];
        if (y_texture == nil || uv_texture == nil || command_buffer == nil || encoder == nil) {
            CFRelease(y_reference);
            CFRelease(uv_reference);
            set_error(error_buffer, error_capacity, "Metal command allocation failed");
            return 5;
        }

        ConversionParams params{
            source_width / 2,
            output_width / 2,
            source_height,
            output_height,
        };
        [encoder setComputePipelineState:converter->pipeline];
        [encoder setTexture:source_texture atIndex:0];
        [encoder setTexture:y_texture atIndex:1];
        [encoder setTexture:uv_texture atIndex:2];
        [encoder setBytes:&params length:sizeof(params) atIndex:0];
        MTLSize grid = MTLSizeMake(output_width / 2, output_height / 2, 1);
        NSUInteger thread_width = converter->pipeline.threadExecutionWidth;
        NSUInteger thread_height =
            converter->pipeline.maxTotalThreadsPerThreadgroup / thread_width;
        MTLSize threads = MTLSizeMake(thread_width, thread_height, 1);
        [encoder dispatchThreads:grid threadsPerThreadgroup:threads];
        [encoder endEncoding];
        [command_buffer commit];
        [command_buffer waitUntilCompleted];
        CFRelease(y_reference);
        CFRelease(uv_reference);
        CVMetalTextureCacheFlush(converter->texture_cache, 0);

        if (command_buffer.status != MTLCommandBufferStatusCompleted) {
            set_error(
                error_buffer,
                error_capacity,
                command_buffer.error.localizedDescription.UTF8String
                    ?: "Metal conversion command failed");
            return 6;
        }
        if (gpu_duration_ns != nullptr && command_buffer.GPUEndTime >= command_buffer.GPUStartTime) {
            *gpu_duration_ns = static_cast<uint64_t>(
                (command_buffer.GPUEndTime - command_buffer.GPUStartTime) * 1'000'000'000.0);
        }
        return 0;
    }
}

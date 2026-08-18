#version 450

// One invocation reduces one rectangular image block. The CPU reads only a
// compact summary: four raw corner words followed by four positional hash
// lanes for each block in row-major grid order.
layout(local_size_x = 64, local_size_y = 1, local_size_z = 1) in;

layout(set = 0, binding = 0, std430) readonly buffer ProbePixels {
    uint words[];
} input_pixels;

layout(set = 0, binding = 1, std430) writeonly buffer ProbeSummary {
    uint words[];
} output_summary;

layout(push_constant) uniform ProbeDigestParams {
    uvec2 extent;
    uvec2 grid;
} params;

uint rotate_left(uint value, uint amount) {
    return (value << amount) | (value >> (32u - amount));
}

void main() {
    uint block_id = gl_GlobalInvocationID.x;
    uint block_count = params.grid.x * params.grid.y;
    if (block_id >= block_count) {
        return;
    }

    uint width = params.extent.x;
    uint height = params.extent.y;
    if (block_id == 0u) {
        uint bottom = (height - 1u) * width;
        output_summary.words[0] = input_pixels.words[0];
        output_summary.words[1] = input_pixels.words[width - 1u];
        output_summary.words[2] = input_pixels.words[bottom];
        output_summary.words[3] = input_pixels.words[bottom + width - 1u];
    }

    uint block_x = block_id % params.grid.x;
    uint block_y = block_id / params.grid.x;

    // Quotient/remainder partitioning avoids overflowing block * extent and
    // gives every block at least one pixel because grid <= extent.
    uint base_width = width / params.grid.x;
    uint extra_width = width % params.grid.x;
    uint x0 = block_x * base_width + min(block_x, extra_width);
    uint x1 = x0 + base_width + uint(block_x < extra_width);

    uint base_height = height / params.grid.y;
    uint extra_height = height % params.grid.y;
    uint y0 = block_y * base_height + min(block_y, extra_height);
    uint y1 = y0 + base_height + uint(block_y < extra_height);

    uvec4 hash = uvec4(
        0x811c9dc5u ^ block_id,
        0x9e3779b9u ^ (block_id * 0x85ebca6bu),
        0x243f6a88u ^ (block_id * 0xc2b2ae35u),
        0xb7e15162u ^ (block_id * 0x27d4eb2du)
    );

    for (uint y = y0; y < y1; ++y) {
        uint row = y * width;
        for (uint x = x0; x < x1; ++x) {
            uint position = row + x;
            uint pixel = input_pixels.words[position];
            uint keyed = pixel ^ (position * 0x9e3779b9u);

            hash.x = (hash.x ^ keyed) * 0x01000193u;
            hash.y = rotate_left(
                hash.y + pixel + position * 0x85ebca6bu,
                11u
            ) * 0xc2b2ae35u;
            hash.z = rotate_left(
                hash.z ^ (pixel + position * 0x27d4eb2du),
                13u
            ) * 0x165667b1u;
            hash.w = rotate_left(
                hash.w + (pixel ^ rotate_left(position, 16u)),
                17u
            ) * 0x85ebca6bu;
        }
    }

    uint output_index = 4u + block_id * 4u;
    output_summary.words[output_index + 0u] = hash.x;
    output_summary.words[output_index + 1u] = hash.y;
    output_summary.words[output_index + 2u] = hash.z;
    output_summary.words[output_index + 3u] = hash.w;
}

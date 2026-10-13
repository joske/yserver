#version 450

// One invocation compares one tile. The CPU selects the global first
// differing pixel from the per-tile summaries.
layout(local_size_x = 64, local_size_y = 1, local_size_z = 1) in;

layout(set = 0, binding = 0, std430) readonly buffer CandidatePixels {
    uint words[];
} candidate;

layout(set = 0, binding = 1, std430) readonly buffer ReferencePixels {
    uint words[];
} reference;

layout(set = 0, binding = 2, std430) writeonly buffer CompareSummary {
    uint words[];
} summary;

layout(push_constant) uniform DamageAuditCompareParams {
    uvec2 extent;
    uvec2 grid;
} params;

void main() {
    uint tile_id = gl_GlobalInvocationID.x;
    uint tile_count = params.grid.x * params.grid.y;
    if (tile_id >= tile_count) {
        return;
    }

    uint width = params.extent.x;
    uint block_x = tile_id % params.grid.x;
    uint block_y = tile_id / params.grid.x;

    uint base_width = width / params.grid.x;
    uint extra_width = width % params.grid.x;
    uint x0 = block_x * base_width + min(block_x, extra_width);
    uint x1 = x0 + base_width + uint(block_x < extra_width);

    uint base_height = params.extent.y / params.grid.y;
    uint extra_height = params.extent.y % params.grid.y;
    uint y0 = block_y * base_height + min(block_y, extra_height);
    uint y1 = y0 + base_height + uint(block_y < extra_height);

    uint mismatch_count = 0u;
    uint first_index = 0xffffffffu;
    uint first_candidate = 0u;
    uint first_reference = 0u;

    for (uint y = y0; y < y1; ++y) {
        uint row = y * width;
        for (uint x = x0; x < x1; ++x) {
            uint index = row + x;
            uint c = candidate.words[index];
            uint r = reference.words[index];
            if (c != r) {
                mismatch_count += 1u;
                if (first_index == 0xffffffffu) {
                    first_index = index;
                    first_candidate = c;
                    first_reference = r;
                }
            }
        }
    }

    uint out_index = tile_id * 4u;
    summary.words[out_index + 0u] = mismatch_count;
    summary.words[out_index + 1u] = first_index;
    summary.words[out_index + 2u] = first_candidate;
    summary.words[out_index + 3u] = first_reference;
}

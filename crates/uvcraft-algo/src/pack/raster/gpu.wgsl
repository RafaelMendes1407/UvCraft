struct Search {
    dimensions: vec4<u32>, // grid stride, mask stride, mask height, first set x
    bounds: vec4<u32>, // max x, max y, candidate columns, candidate count
};

@group(0) @binding(0) var<storage, read> grid: array<u32>;
@group(0) @binding(1) var<storage, read> mask: array<u32>;
@group(0) @binding(2) var<storage, read_write> best: atomic<u32>;
@group(0) @binding(3) var<uniform> search: Search;

@compute @workgroup_size(256)
fn main(@builtin(global_invocation_id) id: vec3<u32>) {
    let index = id.x;
    if index >= search.bounds.w {
        return;
    }
    let x = index % search.bounds.z;
    let y = index / search.bounds.z;
    let first = x + search.dimensions.w;
    let first_word = grid[y * search.dimensions.x + first / 32u];
    if (first_word & (1u << (first % 32u))) != 0u {
        return;
    }
    let base = x / 32u;
    let shift = x % 32u;
    for (var row = 0u; row < search.dimensions.z; row++) {
        let grid_start = (y + row) * search.dimensions.x;
        let mask_start = row * search.dimensions.y;
        for (var word = 0u; word < search.dimensions.y; word++) {
            let value = mask[mask_start + word];
            if value == 0u {
                continue;
            }
            let location = base + word;
            if location < search.dimensions.x &&
                (grid[grid_start + location] & (value << shift)) != 0u {
                return;
            }
            if shift > 0u && location + 1u < search.dimensions.x &&
                (grid[grid_start + location + 1u] & (value >> (32u - shift))) != 0u {
                return;
            }
        }
    }
    atomicMin(&best, index);
}


#version 460
#extension GL_EXT_scalar_block_layout : enable

layout(push_constant) uniform PushConstants {
    mat4 viewProj;
    uint pos_x_idx;
    uint pos_y_idx;
    uint pos_z_idx;
    uint particle_count;
    float dt;
    uint _padding[11];
} pc;

layout(std430, set = 0, binding = 0) readonly buffer ParticleBuffer {
    float data[];
} particles;

// Tetrahedron corner offsets (pre-multiplied by size)
const vec3 TETRA_CORNERS[4] = vec3[](
    vec3( 1.0,  1.0,  1.0),
    vec3(-1.0, -1.0,  1.0),
    vec3(-1.0,  1.0, -1.0),
    vec3( 1.0, -1.0, -1.0)
);
const float TETRA_SIZE = 15.0;

void main() {
    // 1. Map index buffer ID to particle & local corner
    uint particle_idx = gl_VertexIndex / 4;
    uint corner_idx   = gl_VertexIndex % 4;

    // 2. Fetch particle position from SOA layout
    float px = particles.data[particle_idx];
    float py = particles.data[particle_idx + pc.particle_count];
    float pz = particles.data[particle_idx + (pc.particle_count * 2)];
    vec3 world_pos = vec3(px, py, pz);

    // 3. Generate corner offset
    vec3 corner_offset = TETRA_CORNERS[corner_idx] * TETRA_SIZE;
    vec3 final_pos = world_pos + corner_offset;

    // 4. Transform & output
    gl_Position = pc.viewProj * vec4(final_pos, 1.0);
}

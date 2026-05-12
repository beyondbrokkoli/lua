#version 460

layout(std430, binding = 0) readonly buffer MegaBuffer {
    float data[];
};

layout(push_constant) uniform PushConstants {
    uint pos_x_idx;
    uint pos_y_idx;
    uint pos_z_idx;
    uint particle_count;
    float dt;
} pc;

// --> NEW: Explicit output to Fragment Shader
layout(location = 0) out vec4 fragColor;

void main() {
    uint id = gl_VertexIndex;

    if (id >= pc.particle_count) {
        gl_Position = vec4(0.0, 0.0, 0.0, 0.0);
        return;
    }

    float x = data[pc.pos_x_idx + id];
    float y = data[pc.pos_y_idx + id];
    float z = data[pc.pos_z_idx + id];

    vec2 screen_pos = vec2(x / 400.0, y / 400.0);

    gl_Position = vec4(screen_pos, 0.5, 1.0);
    gl_PointSize = 2.0; 
    
    // --> NEW: Write out a default color to satisfy the Fragment Shader
    fragColor = vec4(1.0, 1.0, 1.0, 1.0); 
}

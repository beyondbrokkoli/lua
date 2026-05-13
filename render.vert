#version 460

layout(std430, binding = 0) readonly buffer MegaBuffer {
    float data[];
};

layout(push_constant) uniform PushConstants {
    mat4 viewProj;
    uint pos_x_idx;
    uint pos_y_idx;
    uint pos_z_idx;
    uint particle_count;
    float dt;
} pc;

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

    // STANDARD LEFT-MULTIPLY: Hardware-native math evaluation
    gl_Position = pc.viewProj * vec4(x, y, z, 1.0);
    gl_PointSize = 2.0;

    float depth_intensity = clamp((z + 300.0) / 600.0, 0.2, 1.0);
    fragColor = vec4(depth_intensity, depth_intensity * 0.8, 1.0, 1.0);
}

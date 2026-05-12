#version 460

// The exact same Unified VRAM Arena from the Compute Shader
layout(std430, binding = 0) readonly buffer MegaBuffer {
    float data[];
};

// The 64-Byte Router Matrix
layout(push_constant) uniform PushConstants {
    uint pos_x_idx;
    uint pos_y_idx;
    uint pos_z_idx;
    uint particle_count;
    float dt;
} pc;

// Define a simple camera/projection matrix (you can pass this via UBO later, using orthographic for testing)
void main() {
    // gl_VertexIndex is natively provided by Vulkan during a draw call
    uint id = gl_VertexIndex;
    
    // Safety check
    if (id >= pc.particle_count) {
        gl_Position = vec4(0.0, 0.0, 0.0, 0.0);
        return;
    }

    // Programmable Vertex Pulling from the SoA Mega-Buffer
    float x = data[pc.pos_x_idx + id];
    float y = data[pc.pos_y_idx + id];
    float z = data[pc.pos_z_idx + id];

    // Simple scale/translation to make them visible on screen 
    // (Assuming coordinates are in a 200x200x200 grid from your compute shader)
    vec2 screen_pos = vec2(x / 400.0, y / 400.0); 

    gl_Position = vec4(screen_pos, 0.5, 1.0);
    gl_PointSize = 2.0; // Requires deviceFeatures.largePoints = 1 (which we enabled!)
}

#version 460

layout(set = 0, binding = 0) readonly buffer MasterBuffer {
    float data[];
} vram;

// Must map perfectly to the 128-byte Lua PushConstants struct
layout(push_constant) uniform PushConstants {
    mat4 viewProj;
    uint pos_x_idx;
    uint pos_y_idx;
    uint pos_z_idx;
    uint particle_count;
    float dt; 
    uint vel_x_idx;
    uint vel_y_idx;
    uint vel_z_idx;
    uint target_state; // Offset 96: Our magic shape/color flag!
} pc;

layout(location = 0) out vec3 fragColor;
layout(location = 1) out vec3 v_worldPos;
layout(location = 2) flat out uint v_shapeID; // Pass to frag for specific lighting

// The Mega-Library of Vertices
const vec3 SHAPE_LIBRARY[14] = vec3[](
    // --- SHAPE 0: ICE SHARD (Vertices 0 to 5) ---
    vec3(0.0,  1.5,  0.0), // 0: Top
    vec3(0.0, -0.5,  0.0), // 1: Bottom
    vec3(-1.0, 0.0,  1.0), // 2: Front-Left
    vec3( 1.0, 0.0,  1.0), // 3: Front-Right
    vec3( 1.0, 0.0, -1.0), // 4: Back-Right
    vec3(-1.0, 0.0, -1.0), // 5: Back-Left

    // --- SHAPE 1: OBSIDIAN CUBE (Vertices 6 to 13) ---
    vec3(-1.0, -1.0,  1.0), // 6: Front Bottom Left
    vec3( 1.0, -1.0,  1.0), // 7: Front Bottom Right
    vec3( 1.0,  1.0,  1.0), // 8: Front Top Right
    vec3(-1.0,  1.0,  1.0), // 9: Front Top Left
    vec3(-1.0, -1.0, -1.0), // 10: Back Bottom Left
    vec3( 1.0, -1.0, -1.0), // 11: Back Bottom Right
    vec3( 1.0,  1.0, -1.0), // 12: Back Top Right
    vec3(-1.0,  1.0, -1.0)  // 13: Back Top Left
);

float hash(uint n) {
    n = (n << 13U) ^ n;
    n = n * (n * n * 15731U + 789221U) + 1376312589U;
    return float(n & uvec3(0x7fffffffU)) / float(0x7fffffff);
}

mat3 rotate3D(float x, float y, float z) {
    float cx = cos(x), sx = sin(x);
    float cy = cos(y), sy = sin(y);
    float cz = cos(z), sz = sin(z);
    return mat3(
        cy*cz, -cx*sz + sx*sy*cz,  sx*sz + cx*sy*cz,
        cy*sz,  cx*cz + sx*sy*sz, -sx*cz + cx*sy*sz,
       -sy,     sx*cy,             cx*cy
    );
}

void main() {
    uint p_id = gl_InstanceIndex;

    // Fetch CPU Macro-State
    vec3 anchor = vec3(
        vram.data[pc.pos_x_idx + p_id],
        vram.data[pc.pos_y_idx + p_id],
        vram.data[pc.pos_z_idx + p_id]
    );

    // MAGIC: gl_VertexIndex is now driven by your Lua Index Buffer!
    vec3 local_pos = SHAPE_LIBRARY[gl_VertexIndex];

    float h1 = hash(p_id);
    float h2 = hash(p_id + 1337);
    float h3 = hash(p_id + 42069);

    vec3 scale = vec3(0.5 + h1 * 1.5, 0.5 + h2 * 3.0, 0.5 + h3 * 1.5) * 500.0;

    // Make the Asteroid Cubes absolute massive units
    if (pc.target_state == 99) {
        scale *= 1.8; 
    }

    local_pos *= scale;

    float speed_x = (h1 - 0.5) * 0.8;
    float speed_y = (h2 - 0.5) * 0.8;
    float speed_z = (h3 - 0.5) * 0.8;

    float rx = (h1 * 6.28) + (pc.dt * speed_x);
    float ry = (h2 * 6.28) + (pc.dt * speed_y);
    float rz = (h3 * 6.28) + (pc.dt * speed_z);

    local_pos = rotate3D(rx, ry, rz) * local_pos;
    anchor.y += sin(pc.dt * 0.5 + h1 * 6.28) * 150.0;

    vec3 final_world_pos = anchor + local_pos;
    gl_Position = pc.viewProj * vec4(final_world_pos, 1.0);
    v_worldPos = final_world_pos;
    v_shapeID = pc.target_state;

    // SPATIAL COLOR DISTRIBUTION (MERIDIAN DOMINANCE)
    // Generate a massive, sweeping 3D wave through the world coordinates
    float spatial_wave = sin(anchor.x * 0.00015) * cos(anchor.z * 0.00015) * sin(anchor.y * 0.0001);
    float norm_wave = spatial_wave * 0.5 + 0.5; // Scale to 0.0 -> 1.0

    // Much richer, deeper Meridian Blue
    vec3 c_meridian = vec3(0.0, 0.35, 0.75); 
    vec3 c_cyan     = vec3(0.15, 0.75, 0.90); 
    vec3 c_ice      = vec3(0.95, 0.98, 1.00); 

    vec3 swarm_color;
    
    // Shift the balance: Meridian Blue now claims 70% of the spatial wave
    if (norm_wave < 0.7) {
        // Smoothly mix from Meridian to Cyan over the 0.0 -> 0.7 range
        swarm_color = mix(c_meridian, c_cyan, norm_wave * (1.0 / 0.7));
    } else {
        // Fast, sharp transition to Ice White for the peaks (0.7 -> 1.0)
        swarm_color = mix(c_cyan, c_ice, (norm_wave - 0.7) * (1.0 / 0.3));
    }

    // Inject per-particle noise so it looks natural
    swarm_color = mix(swarm_color, vec3(h1, h2, h3), 0.15);

    // Override color for the Asteroid Cubes (DEATH TO PURPLE)
    if (pc.target_state == 99) {
        // Now they are dark, abyssal slate/teal
        swarm_color = mix(vec3(0.04, 0.06, 0.10), vec3(0.10, 0.25, 0.35), h2);
    }

    fragColor = swarm_color;
}

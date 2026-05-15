#version 460
layout(location = 0) out vec4 outColor;

void main() {
    // ✅ Simple solid color (or add depth-based fading if desired)
    outColor = vec4(0.1, 0.1, 0.9, 1.0);
    
    // ❌ REMOVE any gl_PointCoord usage:
    // outColor = texture(pointSprite, gl_PointCoord); // ← Delete this
}

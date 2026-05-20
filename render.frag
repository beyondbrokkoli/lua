#version 460

layout(location = 0) in vec3 fragColor;
layout(location = 1) in vec3 v_worldPos;
layout(location = 2) flat in uint v_shapeID;

layout(location = 0) out vec4 outColor;

void main() {
    // Procedural Flat Normals
    vec3 dpdx = dFdx(v_worldPos);
    vec3 dpdy = dFdy(v_worldPos);
    vec3 normal = normalize(cross(dpdx, dpdy));

    vec3 lightDir = normalize(vec3(0.5, 1.0, 0.8));
    vec3 viewDir = vec3(0.0, 0.0, 1.0);
    vec3 halfDir = normalize(lightDir + viewDir);

    // Diffuse Shading
    float diffuse = max(dot(normal, lightDir), 0.15);
    vec3 litColor = fragColor * diffuse;

    // Specular Highlights (Faceted Ice vs Dull Obsidian)
    float specPower = 64.0;
    float specIntensity = 1.2;
    vec3 specTint = vec3(1.0);

    if (v_shapeID == 99) {
        // Obsidian Cubes: Dull, broad specular highlights
        specPower = 16.0;
        specIntensity = 0.3;
        // Replaced the lilac sheen with an icy, deep-sea cyan reflection
        specTint = vec3(0.4, 0.8, 1.0); 
    }

    float specular = pow(max(dot(normal, halfDir), 0.0), specPower) * specIntensity;
    litColor += (specTint * specular);

    outColor = vec4(litColor, 1.0);
}

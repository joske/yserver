#version 450

// Full-screen triangle for the RANDR CRTC transform scale pass. Three
// vertices from gl_VertexIndex, no vertex buffer.

void main() {
    vec2 p = vec2(float((gl_VertexIndex << 1) & 2), float(gl_VertexIndex & 2));
    gl_Position = vec4(p * 2.0 - 1.0, 0.0, 1.0);
}

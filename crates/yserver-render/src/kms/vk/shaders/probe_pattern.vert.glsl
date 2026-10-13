#version 450

// Full-screen triangle for copied-scanout transport probing. The oversized
// triangle avoids an internal diagonal seam and needs no vertex buffer.

void main() {
    vec2 corner = vec2(float((gl_VertexIndex << 1) & 2),
                       float(gl_VertexIndex & 2));
    gl_Position = vec4(corner * 2.0 - 1.0, 0.0, 1.0);
}

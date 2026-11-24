// The vertex stage of the core's OCIO pass (src/ocio): one triangle over the
// whole target. build.rs compiles it to SPIR-V. The fragment stage is made
// at run time, around the shader OCIO generates.

[shader("vertex")]
float4 vs(uint index : SV_VulkanVertexID) : SV_Position {
    float2 corner = float2(float((index << 1) & 2), float(index & 2));
    return float4(corner * 2.0 - 1.0, 0.0, 1.0);
}

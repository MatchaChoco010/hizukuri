// A module for the tests: a type whose state is internal to the module and
// can only be used through its methods.
module tint;

public struct Tinted {
    internal float4 factor;

    public __init(float4 factor) {
        this.factor = factor;
    }

    public float4 apply(float4 color) {
        return color * factor;
    }
};

local ffi = require("ffi")
local math = require("math")
local vmath = {}

-- Define 16-byte aligned structs for SIMD readiness in C
ffi.cdef[[
    typedef struct __attribute__((aligned(16))) { float x, y, z, w; } vec4_t;
    typedef struct __attribute__((aligned(64))) { float m[16]; } mat4_t;
]]

-- Pre-allocate ALL workspace memory to strictly prevent Garbage Collection
local temp_f   = ffi.new("vec4_t")
local temp_u   = ffi.new("vec4_t")
local temp_r   = ffi.new("vec4_t")
local temp_mat = ffi.new("mat4_t")

function vmath.lookAt(eye_x, eye_y, eye_z, center_x, center_y, center_z, out_mat)
    -- Forward Vector
    temp_f.x = center_x - eye_x; temp_f.y = center_y - eye_y; temp_f.z = center_z - eye_z
    local f_inv = 1.0 / math.sqrt(temp_f.x^2 + temp_f.y^2 + temp_f.z^2)
    temp_f.x = temp_f.x * f_inv; temp_f.y = temp_f.y * f_inv; temp_f.z = temp_f.z * f_inv

    -- Gimbal Lock Prevention
    local up_x = 0.0; local up_y = 1.0; local up_z = 0.0
    if math.abs(temp_f.x) < 0.001 and math.abs(temp_f.z) < 0.001 then
        if temp_f.y > 0 then up_z = -1.0 else up_z = 1.0 end
        up_y = 0.0; up_x = 0.0
    end

    -- Right Vector
    temp_r.x = up_y * temp_f.z - up_z * temp_f.y
    temp_r.y = up_z * temp_f.x - up_x * temp_f.z
    temp_r.z = up_x * temp_f.y - up_y * temp_f.x
    local r_inv = 1.0 / math.sqrt(temp_r.x^2 + temp_r.y^2 + temp_r.z^2)
    temp_r.x = temp_r.x * r_inv; temp_r.y = temp_r.y * r_inv; temp_r.z = temp_r.z * r_inv

    -- True Up Vector
    temp_u.x = temp_f.y * temp_r.z - temp_f.z * temp_r.y
    temp_u.y = temp_f.z * temp_r.x - temp_f.x * temp_r.z
    temp_u.z = temp_f.x * temp_r.y - temp_f.y * temp_r.x

    -- NATIVE COLUMN-MAJOR MEMORY WRITE (Sober Edition)
    -- Column 0 (Rx, Ux, -Fx, 0)
    out_mat.m[0]  = temp_r.x;  out_mat.m[1]  = temp_u.x;  out_mat.m[2]  = -temp_f.x; out_mat.m[3]  = 0.0
    -- Column 1 (Ry, Uy, -Fy, 0)
    out_mat.m[4]  = temp_r.y;  out_mat.m[5]  = temp_u.y;  out_mat.m[6]  = -temp_f.y; out_mat.m[7]  = 0.0
    -- Column 2 (Rz, Uz, -Fz, 0)
    out_mat.m[8]  = temp_r.z;  out_mat.m[9]  = temp_u.z;  out_mat.m[10] = -temp_f.z; out_mat.m[11] = 0.0
    -- Column 3 (Tx, Ty, Tz, 1)
    out_mat.m[12] = -(temp_r.x*eye_x + temp_r.y*eye_y + temp_r.z*eye_z)
    out_mat.m[13] = -(temp_u.x*eye_x + temp_u.y*eye_y + temp_u.z*eye_z)
    out_mat.m[14] =  (temp_f.x*eye_x + temp_f.y*eye_y + temp_f.z*eye_z)
    out_mat.m[15] = 1.0
end

function vmath.perspective_inf_revz(fov_degrees, aspect, near, out_mat)
    local f = 1.0 / math.tan(math.rad(fov_degrees) * 0.5)
    
    -- NATIVE COLUMN-MAJOR REVERSE-Z
    out_mat.m[0] = f / aspect; out_mat.m[4] = 0.0; out_mat.m[8]  = 0.0;  out_mat.m[12] = 0.0
    out_mat.m[1] = 0.0;        out_mat.m[5] = -f;  out_mat.m[9]  = 0.0;  out_mat.m[13] = 0.0
    out_mat.m[2] = 0.0;        out_mat.m[6] = 0.0; out_mat.m[10] = 0.0;  out_mat.m[14] = near
    out_mat.m[3] = 0.0;        out_mat.m[7] = 0.0; out_mat.m[11] = -1.0; out_mat.m[15] = 0.0
end

function vmath.multiply_mat4(a, b, out_mat)
    -- NATIVE COLUMN-MAJOR MATRIX MULTIPLICATION
    for col = 0, 3 do
        for row = 0, 3 do
            temp_mat.m[col*4 + row] = a.m[0*4 + row] * b.m[col*4 + 0] +
                                      a.m[1*4 + row] * b.m[col*4 + 1] +
                                      a.m[2*4 + row] * b.m[col*4 + 2] +
                                      a.m[3*4 + row] * b.m[col*4 + 3]
        end
    end
    for k = 0, 15 do
        out_mat.m[k] = temp_mat.m[k]
    end
end

return vmath

-- vmath.lua
local ffi = require("ffi")
local math = require("math")
local vmath = {}

function vmath.perspective_inf_revz(fov_degrees, aspect, near, out)
    local f = 1.0 / math.tan(math.rad(fov_degrees) * 0.5)
    
    out[0]  = f / aspect; out[4]  = 0.0; out[8]  = 0.0;  out[12] = 0.0
    out[1]  = 0.0;        out[5]  = -f;  out[9]  = 0.0;  out[13] = 0.0
    out[2]  = 0.0;        out[6]  = 0.0; out[10] = 0.0;  out[14] = near
    out[3]  = 0.0;        out[7]  = 0.0; out[11] = -1.0; out[15] = 0.0
end

function vmath.lookAt(eye_x, eye_y, eye_z, center_x, center_y, center_z, out)
    local fx = center_x - eye_x
    local fy = center_y - eye_y
    local fz = center_z - eye_z
    local f_inv = 1.0 / math.sqrt(fx*fx + fy*fy + fz*fz)
    fx = fx * f_inv; fy = fy * f_inv; fz = fz * f_inv

    local rx = fz; local ry = 0.0; local rz = -fx
    local r_inv = 1.0 / math.sqrt(rx*rx + ry*ry + rz*rz)
    rx = rx * r_inv; ry = ry * r_inv; rz = rz * r_inv

    local ux = ry * fz - rz * fy
    local uy = rz * fx - rx * fz
    local uz = rx * fy - ry * fx

    out[0] = rx; out[4] = ry; out[8]  = rz; out[12] = -(rx*eye_x + ry*eye_y + rz*eye_z)
    out[1] = ux; out[5] = uy; out[9]  = uz; out[13] = -(ux*eye_x + uy*eye_y + uz*eye_z)
    out[2] =-fx; out[6] =-fy; out[10] =-fz; out[14] =  (fx*eye_x + fy*eye_y + fz*eye_z)
    out[3] = 0.0; out[7] = 0.0; out[11] = 0.0; out[15] = 1.0
end

function vmath.multiply_mat4(a, b, out)
    for i = 0, 3 do
        for j = 0, 3 do
            out[i*4 + j] = a[i*4 + 0] * b[0*4 + j] +
                           a[i*4 + 1] * b[1*4 + j] +
                           a[i*4 + 2] * b[2*4 + j] +
                           a[i*4 + 3] * b[3*4 + j]
        end
    end
end

return vmath

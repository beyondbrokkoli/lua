#include <immintrin.h>
#include <math.h>
#include <stdint.h>
#include <stdlib.h>
#include <stdio.h>

#ifdef _WIN32
    // Windows DLL export
    #define EXPORT __declspec(dllexport)
#else
    // Linux/macOS Shared Object export
    #define EXPORT __attribute__((visibility("default")))
#endif

#if defined(_WIN32) || defined(_WIN64)
    #include <windows.h>
    typedef HANDLE vmath_thread_t;
    typedef CRITICAL_SECTION vmath_mutex_t;
    typedef CONDITION_VARIABLE vmath_cond_t;
    #define THREAD_FUNC DWORD WINAPI
    #define THREAD_RETURN_VAL 0
    static vmath_thread_t vmath_thread_start(DWORD (WINAPI *func)(LPVOID), void* arg) { return CreateThread(NULL, 0, func, arg, 0, NULL); }
    static void vmath_thread_join(vmath_thread_t thread) { WaitForSingleObject(thread, INFINITE); CloseHandle(thread); }
    static void vmath_mutex_init(vmath_mutex_t* m) { InitializeCriticalSection(m); }
    static void vmath_mutex_lock(vmath_mutex_t* m) { EnterCriticalSection(m); }
    static void vmath_mutex_unlock(vmath_mutex_t* m) { LeaveCriticalSection(m); }
    static void vmath_mutex_destroy(vmath_mutex_t* m) { DeleteCriticalSection(m); }
    static void vmath_cond_init(vmath_cond_t* cv) { InitializeConditionVariable(cv); }
    static void vmath_cond_wait(vmath_cond_t* cv, vmath_mutex_t* m) { SleepConditionVariableCS(cv, m, INFINITE); }
    static void vmath_cond_broadcast(vmath_cond_t* cv) { WakeAllConditionVariable(cv); }
    static void vmath_cond_destroy(vmath_cond_t* cv) { }
#else
    #include <pthread.h>
    typedef pthread_t vmath_thread_t;
    typedef pthread_mutex_t vmath_mutex_t;
    typedef pthread_cond_t vmath_cond_t;
    #define THREAD_FUNC void*
    #define THREAD_RETURN_VAL NULL
    static vmath_thread_t vmath_thread_start(void* (*func)(void*), void* arg) { pthread_t thread; pthread_create(&thread, NULL, func, arg); return thread; }
    static void vmath_thread_join(vmath_thread_t thread) { pthread_join(thread, NULL); }
    static void vmath_mutex_init(vmath_mutex_t* m) { pthread_mutex_init(m, NULL); }
    static void vmath_mutex_lock(vmath_mutex_t* m) { pthread_mutex_lock(m); }
    static void vmath_mutex_unlock(vmath_mutex_t* m) { pthread_mutex_unlock(m); }
    static void vmath_mutex_destroy(vmath_mutex_t* m) { pthread_mutex_destroy(m); }
    static void vmath_cond_init(vmath_cond_t* cv) { pthread_cond_init(cv, NULL); }
    static void vmath_cond_wait(vmath_cond_t* cv, vmath_mutex_t* m) { pthread_cond_wait(cv, m); }
    static void vmath_cond_broadcast(vmath_cond_t* cv) { pthread_cond_broadcast(cv); }
    static void vmath_cond_destroy(vmath_cond_t* cv) { pthread_cond_destroy(cv); }
#endif

#ifndef M_PI
#define M_PI 3.14159265358979323846f
#endif

// [Insert wrap_pi_avx, fast_sin_avx, fast_cos_avx, fast_trig_noise_avx]
static inline __m256 wrap_pi_avx(__m256 x) {
    __m256 inv_two_pi = _mm256_set1_ps(1.0f / (2.0f * M_PI));
    __m256 two_pi = _mm256_set1_ps(2.0f * M_PI);
    __m256 q = _mm256_round_ps(_mm256_mul_ps(x, inv_two_pi), _MM_FROUND_TO_NEAREST_INT | _MM_FROUND_NO_EXC);
    return _mm256_fnmadd_ps(q, two_pi, x);
}
static inline __m256 fast_sin_avx(__m256 x) {
    x = wrap_pi_avx(x);
    __m256 B = _mm256_set1_ps(4.0f / M_PI), C = _mm256_set1_ps(-4.0f / (M_PI * M_PI));
    __m256 x_abs = _mm256_andnot_ps(_mm256_set1_ps(-0.0f), x);
    __m256 y = _mm256_fmadd_ps(_mm256_mul_ps(C, x_abs), x, _mm256_mul_ps(B, x));
    __m256 P = _mm256_set1_ps(0.225f);
    __m256 y_abs = _mm256_andnot_ps(_mm256_set1_ps(-0.0f), y);
    return _mm256_fmadd_ps(_mm256_fmadd_ps(y_abs, y, _mm256_sub_ps(_mm256_setzero_ps(), y)), P, y);
}
static inline __m256 fast_cos_avx(__m256 x) { return fast_sin_avx(_mm256_add_ps(x, _mm256_set1_ps(M_PI / 2.0f))); }
static inline __m256 fast_trig_noise_avx(__m256 nx, __m256 ny, __m256 nz, __m256 time) {
    __m256 v1 = fast_sin_avx(_mm256_add_ps(_mm256_mul_ps(nx, _mm256_set1_ps(3.1f)), time));
    __m256 v2 = fast_cos_avx(_mm256_add_ps(_mm256_mul_ps(ny, _mm256_set1_ps(2.8f)), time));
    __m256 v3 = fast_sin_avx(_mm256_add_ps(_mm256_mul_ps(nz, _mm256_set1_ps(3.4f)), time));
    __m256 out = _mm256_add_ps(v1, _mm256_add_ps(v2, v3));
    __m256 time2 = _mm256_mul_ps(time, _mm256_set1_ps(1.8f));
    __m256 v4 = fast_sin_avx(_mm256_add_ps(_mm256_mul_ps(nx, _mm256_set1_ps(7.2f)), time2));
    __m256 v5 = fast_cos_avx(_mm256_add_ps(_mm256_mul_ps(ny, _mm256_set1_ps(6.5f)), time2));
    __m256 v6 = fast_sin_avx(_mm256_add_ps(_mm256_mul_ps(nz, _mm256_set1_ps(8.1f)), time2));
    __m256 oct2 = _mm256_mul_ps(_mm256_add_ps(v4, _mm256_add_ps(v5, v6)), _mm256_set1_ps(0.35f));
    return _mm256_mul_ps(_mm256_add_ps(out, oct2), _mm256_set1_ps(0.25f));
}
// [Insert APPLY_SPRING_PHYSICS() macro from the OLD TRUTH]
// Boilerplate Spring Physics Macro to keep the shape functions perfectly clean
#define APPLY_SPRING_PHYSICS() \
    __m256 v_px = _mm256_loadu_ps(&px[i]), v_py = _mm256_loadu_ps(&py[i]), v_pz = _mm256_loadu_ps(&pz[i]); \
    __m256 v_vx = _mm256_loadu_ps(&vx[i]), v_vy = _mm256_loadu_ps(&vy[i]), v_vz = _mm256_loadu_ps(&vz[i]); \
    v_vx = _mm256_mul_ps(_mm256_fmadd_ps(_mm256_sub_ps(v_tx, v_px), v_k, v_vx), v_damp); \
    v_vy = _mm256_mul_ps(_mm256_fmadd_ps(_mm256_sub_ps(v_ty, v_py), v_k, v_vy), v_damp); \
    v_vz = _mm256_mul_ps(_mm256_fmadd_ps(_mm256_sub_ps(v_tz, v_pz), v_k, v_vz), v_damp); \
    _mm256_storeu_ps(&px[i], _mm256_fmadd_ps(v_vx, v_dt, v_px)); \
    _mm256_storeu_ps(&py[i], _mm256_fmadd_ps(v_vy, v_dt, v_py)); \
    _mm256_storeu_ps(&pz[i], _mm256_fmadd_ps(v_vz, v_dt, v_pz)); \
    _mm256_storeu_ps(&vx[i], v_vx); _mm256_storeu_ps(&vy[i], v_vy); _mm256_storeu_ps(&vz[i], v_vz);
// [Insert AVX2_BOUNDS_CHECK() macro from the NEW BASELINE]
// Macro to eliminate boilerplate for identical axis bounds checking
#define AVX2_BOUNDS_CHECK(POS, VEL, V_MIN, V_MAX) \
    do { \
        __m256 mask_lt = _mm256_cmp_ps(POS, V_MIN, _CMP_LT_OQ); \
        __m256 mask_gt = _mm256_cmp_ps(POS, V_MAX, _CMP_GT_OQ); \
        __m256 abs_vel = _mm256_and_ps(VEL, v_abs_mask); \
        VEL = _mm256_blendv_ps(VEL, _mm256_mul_ps(abs_vel, v_bounce), mask_lt); \
        VEL = _mm256_blendv_ps(VEL, _mm256_mul_ps(abs_vel, v_neg_bounce), mask_gt); \
        POS = _mm256_max_ps(V_MIN, _mm256_min_ps(V_MAX, POS)); \
    } while(0)

// 3. ISOLATED SIMD KERNELS (Static, no locks, purely mathematical)

// Replace EXPORT with static inline. These are only called by the workers.
static inline void vmath_swarm_update_velocities(int count,
    float* px_in, float* py_in, float* pz_in,
    float* vx_in, float* vy_in, float* vz_in,
    float* px_out, float* py_out, float* pz_out,
    float* vx_out, float* vy_out, float* vz_out,
    float minX, float maxX, float minY, float maxY, float minZ, float maxZ,
    float dt, float gravity)
{
    __m256 v_dt = _mm256_set1_ps(dt);
    __m256 v_grav_dt = _mm256_set1_ps(gravity * dt);
    __m256 v_damp = _mm256_set1_ps(0.995f);
    __m256 v_bounce = _mm256_set1_ps(0.8f);
    __m256 v_neg_bounce = _mm256_set1_ps(-0.8f);

    __m256 v_minX = _mm256_set1_ps(minX), v_maxX = _mm256_set1_ps(maxX);
    __m256 v_minY = _mm256_set1_ps(minY), v_maxY = _mm256_set1_ps(maxY);
    __m256 v_minZ = _mm256_set1_ps(minZ), v_maxZ = _mm256_set1_ps(maxZ);

    // Hardware trick: Bitwise AND with 0x7FFFFFFF clears the float sign bit, yielding absolute value instantly
    __m256 v_abs_mask = _mm256_castsi256_ps(_mm256_set1_epi32(0x7FFFFFFF));

    int i = 0;
    for (; i <= count - 8; i += 8) {
        // 1. Load (Unaligned assuming standard heap allocations, switch to _mm256_load_ps if 32-byte aligned)
        __m256 px = _mm256_loadu_ps(&px_in[i]);
        __m256 py = _mm256_loadu_ps(&py_in[i]);
        __m256 pz = _mm256_loadu_ps(&pz_in[i]);
        __m256 vx = _mm256_loadu_ps(&vx_in[i]);
        __m256 vy = _mm256_loadu_ps(&vy_in[i]);
        __m256 vz = _mm256_loadu_ps(&vz_in[i]);

        // 2. Physics & Integration
        vy = _mm256_sub_ps(vy, v_grav_dt);
        vx = _mm256_mul_ps(vx, v_damp);
        vy = _mm256_mul_ps(vy, v_damp);
        vz = _mm256_mul_ps(vz, v_damp);

        px = _mm256_fmadd_ps(vx, v_dt, px);
        py = _mm256_fmadd_ps(vy, v_dt, py);
        pz = _mm256_fmadd_ps(vz, v_dt, pz);

        // 3. Hardware Clamping & Reflection
        AVX2_BOUNDS_CHECK(px, vx, v_minX, v_maxX);
        AVX2_BOUNDS_CHECK(py, vy, v_minY, v_maxY);
        AVX2_BOUNDS_CHECK(pz, vz, v_minZ, v_maxZ);

        // 4. Store
        _mm256_storeu_ps(&px_out[i], px);
        _mm256_storeu_ps(&py_out[i], py);
        _mm256_storeu_ps(&pz_out[i], pz);
        _mm256_storeu_ps(&vx_out[i], vx);
        _mm256_storeu_ps(&vy_out[i], vy);
        _mm256_storeu_ps(&vz_out[i], vz);
    }

    // Scalar Tail for N % 8 remainder
    for (; i < count; i++) {
        float px = px_in[i], py = py_in[i], pz = pz_in[i];
        float vx = vx_in[i], vy = vy_in[i], vz = vz_in[i];

        vy -= (gravity * dt);
        vx *= 0.995f; vy *= 0.995f; vz *= 0.995f;
        px += vx * dt; py += vy * dt; pz += vz * dt;

        if (px < minX) { px = minX; vx = fabsf(vx) * 0.8f; } else if (px > maxX) { px = maxX; vx = fabsf(vx) * -0.8f; }
        if (py < minY) { py = minY; vy = fabsf(vy) * 0.8f; } else if (py > maxY) { py = maxY; vy = fabsf(vy) * -0.8f; }
        if (pz < minZ) { pz = minZ; vz = fabsf(vz) * 0.8f; } else if (pz > maxZ) { pz = maxZ; vz = fabsf(vz) * -0.8f; }

        px_out[i] = px; py_out[i] = py; pz_out[i] = pz;
        vx_out[i] = vx; vy_out[i] = vy; vz_out[i] = vz;
    }
}
static inline void vmath_swarm_apply_explosion(int count, float* px, float* py, float* pz, float* vx, float* vy, float* vz, float ex, float ey, float ez, float force, float radius) {
    __m256 v_ex = _mm256_set1_ps(ex), v_ey = _mm256_set1_ps(ey), v_ez = _mm256_set1_ps(ez);
    __m256 v_r2 = _mm256_set1_ps(radius * radius);
    __m256 v_1 = _mm256_set1_ps(1.0f);
    __m256 v_force = _mm256_set1_ps(force);
    __m256 v_inv_radius = _mm256_set1_ps(1.0f / radius);

    int i = 0; // <--- EXTRACTED SO IT SURVIVES FOR THE SCALAR LOOP!
    for (; i <= count - 8; i += 8) {
        __m256 dx = _mm256_sub_ps(_mm256_loadu_ps(&px[i]), v_ex);
        __m256 dy = _mm256_sub_ps(_mm256_loadu_ps(&py[i]), v_ey);
        __m256 dz = _mm256_sub_ps(_mm256_loadu_ps(&pz[i]), v_ez);

        __m256 dist2 = _mm256_fmadd_ps(dz, dz, _mm256_fmadd_ps(dy, dy, _mm256_mul_ps(dx, dx)));

        // Mask: 1.0f < dist2 < r2
        __m256 mask = _mm256_and_ps(_mm256_cmp_ps(dist2, v_r2, _CMP_LT_OQ), _mm256_cmp_ps(dist2, v_1, _CMP_GT_OQ));

        if (!_mm256_testz_ps(mask, mask)) {
            __m256 inv_dist = _mm256_rsqrt_ps(dist2); // Fast hardware inverse square root
            __m256 dist = _mm256_mul_ps(dist2, inv_dist);

            // f = force * (1.0f - dist * inv_radius)
            __m256 f = _mm256_mul_ps(v_force, _mm256_sub_ps(v_1, _mm256_mul_ps(dist, v_inv_radius)));
            __m256 f_inv_dist = _mm256_mul_ps(f, inv_dist); // (f / dist)

            __m256 v_vx = _mm256_loadu_ps(&vx[i]);
            __m256 v_vy = _mm256_loadu_ps(&vy[i]);
            __m256 v_vz = _mm256_loadu_ps(&vz[i]);

            v_vx = _mm256_blendv_ps(v_vx, _mm256_fmadd_ps(dx, f_inv_dist, v_vx), mask);
            v_vy = _mm256_blendv_ps(v_vy, _mm256_fmadd_ps(dy, f_inv_dist, v_vy), mask);
            v_vz = _mm256_blendv_ps(v_vz, _mm256_fmadd_ps(dz, f_inv_dist, v_vz), mask);

            _mm256_storeu_ps(&vx[i], v_vx);
            _mm256_storeu_ps(&vy[i], v_vy);
            _mm256_storeu_ps(&vz[i], v_vz);
        }
    }
    // ========================================================
    // SCALAR TAIL LOOP (For Safety - Mod 8 remainders)
    // ========================================================
    float inv_radius = 1.0f / radius;
    float r2 = radius * radius;

    for (; i < count; i++) {
        float dx = px[i] - ex;
        float dy = py[i] - ey;
        float dz = pz[i] - ez;

        float dist2 = dx * dx + dy * dy + dz * dz;

        // Apply only if it's within the blast radius and not exactly at the origin (to prevent divide-by-zero)
        if (dist2 > 1.0f && dist2 < r2) {
            float dist = sqrtf(dist2);

            // Linear falloff: 100% force at center, 0% at edge of radius
            float f = force * (1.0f - dist * inv_radius);

            // Divide by distance once so we can multiply by the raw dx/dy/dz vectors
            float f_inv_dist = f / dist;

            vx[i] += dx * f_inv_dist;
            vy[i] += dy * f_inv_dist;
            vz[i] += dz * f_inv_dist;
        }
    }
}
static inline void vmath_swarm_bundle(int count, float* px, float* py, float* pz, float* vx, float* vy, float* vz, float* seed, float cx, float cy, float cz, float time, float dt) {
    __m256 v_cx = _mm256_set1_ps(cx), v_cy = _mm256_set1_ps(cy), v_cz = _mm256_set1_ps(cz);
    __m256 v_r = _mm256_set1_ps(2000.0f + 400.0f * sinf(time * 6.0f));
    __m256 v_golden = _mm256_set1_ps(2.39996323f);
    __m256 v_1 = _mm256_set1_ps(1.0f), v_2 = _mm256_set1_ps(2.0f);
    __m256 v_dt = _mm256_set1_ps(dt), v_k = _mm256_set1_ps(4.0f * dt), v_damp = _mm256_set1_ps(0.92f);

    int i = 0; // <--- EXTRACTED FOR THE SCALAR TAIL!
    for (; i <= count - 8; i += 8) {
        __m256 v_s = _mm256_loadu_ps(&seed[i]);
        __m256 v_i = _mm256_set_ps(i+7, i+6, i+5, i+4, i+3, i+2, i+1, i);

        __m256 v_phi = _mm256_mul_ps(v_i, v_golden);

        // Math Hack: No acos needed! cos(theta) = 1-2s. sin(theta) = 2*sqrt(s*(1-s))
        __m256 v_cos_theta = _mm256_fnmadd_ps(v_2, v_s, v_1);
        __m256 v_sin_theta = _mm256_mul_ps(v_2, _mm256_sqrt_ps(_mm256_mul_ps(v_s, _mm256_sub_ps(v_1, v_s))));

        __m256 v_tx = _mm256_fmadd_ps(v_r, _mm256_mul_ps(v_sin_theta, fast_cos_avx(v_phi)), v_cx);
        __m256 v_ty = _mm256_fmadd_ps(v_r, v_cos_theta, v_cy);
        __m256 v_tz = _mm256_fmadd_ps(v_r, _mm256_mul_ps(v_sin_theta, fast_sin_avx(v_phi)), v_cz);

        APPLY_SPRING_PHYSICS();
    }
    // ========================================================
    // SCALAR TAIL LOOP (For Safety - Mod 8 remainders)
    // ========================================================
    float r = 2000.0f + 400.0f * sinf(time * 6.0f);
    float golden = 2.39996323f;
    float k = 4.0f * dt;
    float damp = 0.92f;

    for (; i < count; i++) {
        float s = seed[i];
        float phi = (float)i * golden;

        // Math Hack: No acos needed!
        float cos_theta = 1.0f - 2.0f * s;
        float sin_theta = 2.0f * sqrtf(s * (1.0f - s));

        float tx = cx + r * sin_theta * cosf(phi);
        float ty = cy + r * cos_theta;
        float tz = cz + r * sin_theta * sinf(phi);

        // SPRING PHYSICS
        float p_x = px[i], p_y = py[i], p_z = pz[i];
        float v_x = vx[i], v_y = vy[i], v_z = vz[i];

        v_x = (v_x + (tx - p_x) * k) * damp;
        v_y = (v_y + (ty - p_y) * k) * damp;
        v_z = (v_z + (tz - p_z) * k) * damp;

        px[i] = p_x + v_x * dt;
        py[i] = p_y + v_y * dt;
        pz[i] = p_z + v_z * dt;

        vx[i] = v_x;
        vy[i] = v_y;
        vz[i] = v_z;
    }
}

// ... [Insert remaining shape kernels: galaxy, tornado, gyroscope, metal, smales] ...
static inline void vmath_swarm_galaxy(int count, float* px, float* py, float* pz, float* vx, float* vy, float* vz, float* seed, float cx, float cy, float cz, float time, float dt) {
    __m256 v_cx = _mm256_set1_ps(cx), v_cy = _mm256_set1_ps(cy), v_cz = _mm256_set1_ps(cz);
    __m256 v_time_ang = _mm256_set1_ps(time * 1.5f), v_time_z = _mm256_set1_ps(time * 3.0f);
    __m256 v_dt = _mm256_set1_ps(dt), v_k = _mm256_set1_ps(4.0f * dt), v_damp = _mm256_set1_ps(0.92f);

    int i = 0;
    for (; i <= count - 8; i += 8) {
        __m256 v_s = _mm256_loadu_ps(&seed[i]);
        __m256 v_angle = _mm256_fmadd_ps(v_s, _mm256_set1_ps(3.14159f * 30.0f), v_time_ang);
        __m256 v_r = _mm256_fmadd_ps(v_s, _mm256_set1_ps(14000.0f), _mm256_set1_ps(1000.0f));

        __m256 v_tx = _mm256_fmadd_ps(v_r, fast_cos_avx(v_angle), v_cx);
        __m256 v_ty = _mm256_fmadd_ps(_mm256_set1_ps(800.0f), fast_sin_avx(_mm256_fnmadd_ps(v_time_z, _mm256_set1_ps(1.0f), _mm256_mul_ps(v_s, _mm256_set1_ps(40.0f)))), v_cy);
        __m256 v_tz = _mm256_fmadd_ps(v_r, fast_sin_avx(v_angle), v_cz);

        APPLY_SPRING_PHYSICS();
    }
    // ========================================================
    // SCALAR TAIL LOOP (For Safety - Mod 8 remainders)
    // ========================================================
    float k = 4.0f * dt;
    float damp = 0.92f;
    float time_ang = time * 1.5f;
    float time_z = time * 3.0f;

    for (; i < count; i++) {
        float s = seed[i];

        // 1. Calculate Galaxy Arms & Radius
        float angle = s * (3.14159f * 30.0f) + time_ang;
        float r = s * 14000.0f + 1000.0f;

        // 2. Target Positions (with the fnmadd Y-wobble math)
        float tx = cx + r * cosf(angle);
        float ty = cy + 800.0f * sinf((s * 40.0f) - time_z);
        float tz = cz + r * sinf(angle);

        // 3. SPRING PHYSICS
        float p_x = px[i], p_y = py[i], p_z = pz[i];
        float v_x = vx[i], v_y = vy[i], v_z = vz[i];

        v_x = (v_x + (tx - p_x) * k) * damp;
        v_y = (v_y + (ty - p_y) * k) * damp;
        v_z = (v_z + (tz - p_z) * k) * damp;

        px[i] = p_x + v_x * dt;
        py[i] = p_y + v_y * dt;
        pz[i] = p_z + v_z * dt;

        vx[i] = v_x;
        vy[i] = v_y;
        vz[i] = v_z;
    }
}
static inline void vmath_swarm_tornado(int count, float* px, float* py, float* pz, float* vx, float* vy, float* vz, float* seed, float cx, float cy, float cz, float time, float dt) {
    __m256 v_cx = _mm256_set1_ps(cx), v_cy = _mm256_set1_ps(cy), v_cz = _mm256_set1_ps(cz);
    __m256 v_time_ang = _mm256_set1_ps(time * 4.0f);
    __m256 v_dt = _mm256_set1_ps(dt), v_k = _mm256_set1_ps(4.0f * dt), v_damp = _mm256_set1_ps(0.92f);

    int i = 0;
    for (; i <= count - 8; i += 8) {
        __m256 v_s = _mm256_loadu_ps(&seed[i]);
        __m256 v_height = _mm256_fnmadd_ps(_mm256_set1_ps(-24000.0f), v_s, _mm256_set1_ps(-12000.0f));
        __m256 v_angle = _mm256_fnmadd_ps(v_time_ang, _mm256_set1_ps(1.0f), _mm256_mul_ps(v_s, _mm256_set1_ps(3.14159f * 30.0f)));
        __m256 v_r = _mm256_fmadd_ps(v_s, _mm256_set1_ps(4000.0f), _mm256_set1_ps(2000.0f));

        __m256 v_tx = _mm256_fmadd_ps(v_r, fast_cos_avx(v_angle), v_cx);
        __m256 v_ty = _mm256_add_ps(v_cy, v_height);
        __m256 v_tz = _mm256_fmadd_ps(v_r, fast_sin_avx(v_angle), v_cz);

        APPLY_SPRING_PHYSICS();
    }
    // ========================================================
    // SCALAR TAIL LOOP (For Safety - Mod 8 remainders)
    // ========================================================
    float k = 4.0f * dt;
    float damp = 0.92f;
    float time_ang = time * 4.0f;

    for (; i < count; i++) {
        float s = seed[i];

        // 1. Calculate Tornado Structure
        float height = 24000.0f * s - 12000.0f;
        float angle = (s * 3.14159f * 30.0f) - time_ang;
        float r = s * 4000.0f + 2000.0f;

        // 2. Target Positions
        float tx = cx + r * cosf(angle);
        float ty = cy + height;
        float tz = cz + r * sinf(angle);

        // 3. SPRING PHYSICS
        float p_x = px[i], p_y = py[i], p_z = pz[i];
        float v_x = vx[i], v_y = vy[i], v_z = vz[i];

        v_x = (v_x + (tx - p_x) * k) * damp;
        v_y = (v_y + (ty - p_y) * k) * damp;
        v_z = (v_z + (tz - p_z) * k) * damp;

        px[i] = p_x + v_x * dt;
        py[i] = p_y + v_y * dt;
        pz[i] = p_z + v_z * dt;

        vx[i] = v_x;
        vy[i] = v_y;
        vz[i] = v_z;
    }
}
static inline void vmath_swarm_gyroscope(int count, float* px, float* py, float* pz, float* vx, float* vy, float* vz, float* seed, float cx, float cy, float cz, float time, float dt) {
    __m256 v_cx = _mm256_set1_ps(cx), v_cy = _mm256_set1_ps(cy), v_cz = _mm256_set1_ps(cz);
    __m256 v_r = _mm256_set1_ps(7000.0f);
    __m256 v_time_ang = _mm256_set1_ps(time * 2.5f);
    __m256 v_dt = _mm256_set1_ps(dt), v_k = _mm256_set1_ps(4.0f * dt), v_damp = _mm256_set1_ps(0.92f);

    int i = 0;
    for (; i <= count - 8; i += 8) {
        __m256 v_s = _mm256_loadu_ps(&seed[i]);
        __m256 v_angle = _mm256_fmadd_ps(v_s, _mm256_set1_ps(3.14159f * 2.0f), v_time_ang);

        __m256 v_cos = fast_cos_avx(v_angle);
        __m256 v_sin = fast_sin_avx(v_angle);

        // Calculate all 3 ring positions simultaneously!
        __m256 r0_x = _mm256_fmadd_ps(v_r, v_cos, v_cx), r0_y = _mm256_fmadd_ps(v_r, v_sin, v_cy), r0_z = v_cz;
        __m256 r1_x = r0_x, r1_y = v_cy, r1_z = _mm256_fmadd_ps(v_r, v_sin, v_cz);
        __m256 r2_x = v_cx, r2_y = _mm256_fmadd_ps(v_r, v_cos, v_cy), r2_z = r1_z;

        // Masking logic based on (i % 3)
        int rings[8] = { (i)%3, (i+1)%3, (i+2)%3, (i+3)%3, (i+4)%3, (i+5)%3, (i+6)%3, (i+7)%3 };
        __m256i v_ring = _mm256_loadu_si256((__m256i*)rings);

        __m256 m0 = _mm256_castsi256_ps(_mm256_cmpeq_epi32(v_ring, _mm256_setzero_si256()));
        __m256 m1 = _mm256_castsi256_ps(_mm256_cmpeq_epi32(v_ring, _mm256_set1_epi32(1)));

        __m256 v_tx = _mm256_blendv_ps(r2_x, _mm256_blendv_ps(r1_x, r0_x, m0), _mm256_or_ps(m0, m1));
        __m256 v_ty = _mm256_blendv_ps(r2_y, _mm256_blendv_ps(r1_y, r0_y, m0), _mm256_or_ps(m0, m1));
        __m256 v_tz = _mm256_blendv_ps(r2_z, _mm256_blendv_ps(r1_z, r0_z, m0), _mm256_or_ps(m0, m1));

        APPLY_SPRING_PHYSICS();
    }
    // ========================================================
    // SCALAR TAIL LOOP (For Safety - Mod 8 remainders)
    // ========================================================
    float k = 4.0f * dt;
    float damp = 0.92f;
    float time_ang = time * 2.5f;
    float r = 7000.0f;

    for (; i < count; i++) {
        float s = seed[i];
        float angle = (s * 3.14159f * 2.0f) + time_ang;

        float c = cosf(angle);
        float sa = sinf(angle);

        float tx, ty, tz;
        int ring = i % 3;

        // 1. Calculate Target Position based on Ring ID
        if (ring == 0) {
            // Ring 0: XY Plane
            tx = cx + r * c;
            ty = cy + r * sa;
            tz = cz;
        } else if (ring == 1) {
            // Ring 1: XZ Plane
            tx = cx + r * c;
            ty = cy;
            tz = cz + r * sa;
        } else {
            // Ring 2: YZ Plane
            tx = cx;
            ty = cy + r * c;
            tz = cz + r * sa;
        }

        // 2. SPRING PHYSICS
        float p_x = px[i], p_y = py[i], p_z = pz[i];
        float v_x = vx[i], v_y = vy[i], v_z = vz[i];

        v_x = (v_x + (tx - p_x) * k) * damp;
        v_y = (v_y + (ty - p_y) * k) * damp;
        v_z = (v_z + (tz - p_z) * k) * damp;

        px[i] = p_x + v_x * dt;
        py[i] = p_y + v_y * dt;
        pz[i] = p_z + v_z * dt;

        vx[i] = v_x;
        vy[i] = v_y;
        vz[i] = v_z;
    }
}
static inline void vmath_swarm_metal(int count, float* px, float* py, float* pz, float* vx, float* vy, float* vz, float* seed, float cx, float cy, float cz, float time, float dt, float noise_blend) {
    __m256 v_cx = _mm256_set1_ps(cx), v_cy = _mm256_set1_ps(cy), v_cz = _mm256_set1_ps(cz);
    __m256 v_time = _mm256_set1_ps(time);
    __m256 v_blend = _mm256_set1_ps(noise_blend);
    __m256 v_radius = _mm256_set1_ps(4000.0f);
    __m256 v_max_disp = _mm256_set1_ps(3000.0f); // Max noise distortion

    __m256 v_dt = _mm256_set1_ps(dt);
    __m256 v_k = _mm256_set1_ps(4.0f * dt); // Spring stiffness
    __m256 v_damp = _mm256_set1_ps(0.92f);  // Friction

    int i = 0;
    // BLAST 8 PARTICLES PER CYCLE
    for (; i <= count - 8; i += 8) {
        __m256 v_s = _mm256_loadu_ps(&seed[i]);

        // 1. FAST SPHERICAL MAPPING (Fibonacci-style distribution without acos)
        // Z goes from 1.0 to -1.0 based on seed
        __m256 v_sz = _mm256_fnmadd_ps(v_s, _mm256_set1_ps(2.0f), _mm256_set1_ps(1.0f));
        // Radius at this Z: r_xy = sqrt(1.0 - z*z)
        __m256 v_rxy = _mm256_sqrt_ps(_mm256_fnmadd_ps(v_sz, v_sz, _mm256_set1_ps(1.0f)));
        // Phi rotates wildly based on seed
        __m256 v_phi = _mm256_mul_ps(v_s, _mm256_set1_ps(10000.0f));

        __m256 v_sx = _mm256_mul_ps(v_rxy, fast_cos_avx(v_phi));
        __m256 v_sy = _mm256_mul_ps(v_rxy, fast_sin_avx(v_phi));

        // 2. EVALUATE 4D NOISE AT THE NORMALS
        __m256 v_noise = fast_trig_noise_avx(v_sx, v_sy, v_sz, v_time);

        // 3. APPLY DISPLACEMENT (Using FMA to blend seamlessly!)
        // displacement = noise * noise_blend * max_disp
        __m256 v_disp = _mm256_mul_ps(v_noise, _mm256_mul_ps(v_blend, v_max_disp));

        // Target Pos = Center + Normal * (Radius + Displacement)
        __m256 v_final_r = _mm256_add_ps(v_radius, v_disp);
        __m256 v_tx = _mm256_fmadd_ps(v_sx, v_final_r, v_cx);
        __m256 v_ty = _mm256_fmadd_ps(v_sy, v_final_r, v_cy);
        __m256 v_tz = _mm256_fmadd_ps(v_sz, v_final_r, v_cz);

        // 4. SPRING PHYSICS (Pull current pos toward Target Pos)
        __m256 v_px = _mm256_loadu_ps(&px[i]);
        __m256 v_py = _mm256_loadu_ps(&py[i]);
        __m256 v_pz = _mm256_loadu_ps(&pz[i]);

        __m256 v_vx = _mm256_loadu_ps(&vx[i]);
        __m256 v_vy = _mm256_loadu_ps(&vy[i]);
        __m256 v_vz = _mm256_loadu_ps(&vz[i]);

        // v += (target - p) * k * dt; v *= damp;
        v_vx = _mm256_mul_ps(_mm256_fmadd_ps(_mm256_sub_ps(v_tx, v_px), v_k, v_vx), v_damp);
        v_vy = _mm256_mul_ps(_mm256_fmadd_ps(_mm256_sub_ps(v_ty, v_py), v_k, v_vy), v_damp);
        v_vz = _mm256_mul_ps(_mm256_fmadd_ps(_mm256_sub_ps(v_tz, v_pz), v_k, v_vz), v_damp);

        // p += v * dt;
        v_px = _mm256_fmadd_ps(v_vx, v_dt, v_px);
        v_py = _mm256_fmadd_ps(v_vy, v_dt, v_py);
        v_pz = _mm256_fmadd_ps(v_vz, v_dt, v_pz);

        _mm256_storeu_ps(&px[i], v_px);
        _mm256_storeu_ps(&py[i], v_py);
        _mm256_storeu_ps(&pz[i], v_pz);
        _mm256_storeu_ps(&vx[i], v_vx);
        _mm256_storeu_ps(&vy[i], v_vy);
        _mm256_storeu_ps(&vz[i], v_vz);
    }

    // ========================================================
    // SCALAR TAIL LOOP (For Safety - Mod 8 remainders)
    // ========================================================
    for (; i < count; i++) {
        float s = seed[i];

        // 1. FAST SPHERICAL MAPPING
        // _mm256_fnmadd_ps(v_s, 2.0, 1.0) equates to: -(s * 2.0) + 1.0
        float sz = 1.0f - (s * 2.0f);
        float rxy = sqrtf(1.0f - (sz * sz));
        float phi = s * 10000.0f;

        // Standard math is perfectly fine here since it processes 7 particles max!
        float sx = rxy * cosf(phi); 
        float sy = rxy * sinf(phi);

        // 2. EVALUATE 4D NOISE
        // If you wrote a fast_trig_noise_scalar function, call it here!
        // Otherwise, since this runs on <= 7 particles, a standard inline proxy is virtually free:
        float noise = sinf(sx * 10.0f + time) * cosf(sy * 10.0f + time) * sinf(sz * 10.0f + time); 

        // 3. APPLY DISPLACEMENT
        float disp = noise * noise_blend * 3000.0f;
        float final_r = 4000.0f + disp;

        float tx = cx + sx * final_r;
        float ty = cy + sy * final_r;
        float tz = cz + sz * final_r;

        // 4. SPRING PHYSICS
        float p_x = px[i], p_y = py[i], p_z = pz[i];
        float v_x = vx[i], v_y = vy[i], v_z = vz[i];

        float k = 4.0f * dt;
        float damp = 0.92f;

        // v += (target - p) * k * dt; v *= damp;
        v_x = (v_x + (tx - p_x) * k) * damp;
        v_y = (v_y + (ty - p_y) * k) * damp;
        v_z = (v_z + (tz - p_z) * k) * damp;

        // p += v * dt;
        px[i] = p_x + v_x * dt;
        py[i] = p_y + v_y * dt;
        pz[i] = p_z + v_z * dt;

        vx[i] = v_x;
        vy[i] = v_y;
        vz[i] = v_z;
    }
}
static inline void vmath_swarm_smales(int count, float* px, float* py, float* pz, float* vx, float* vy, float* vz, float* seed, float cx, float cy, float cz, float time, float dt, float blend) {
    __m256 v_cx = _mm256_set1_ps(cx), v_cy = _mm256_set1_ps(cy), v_cz = _mm256_set1_ps(cz);
    __m256 v_base_radius = _mm256_set1_ps(4000.0f);

    // THE DOD BLENDING MATH (Calculated once outside the loop!)
    // If blend=0: eversion=1.0, bulge=0.0
    // If blend=1: eversion=cos(t), bulge=sin(t)
    float t_scaled = time * 1.5f;
    float eversion_scalar = 1.0f + blend * (cosf(t_scaled) - 1.0f);
    float bulge_scalar = blend * sinf(t_scaled);

    __m256 v_eversion = _mm256_set1_ps(eversion_scalar);
    __m256 v_bulge = _mm256_set1_ps(bulge_scalar);

    __m256 v_1_2 = _mm256_set1_ps(1.2f);
    __m256 v_0_5 = _mm256_set1_ps(0.5f);
    __m256 v_4_0 = _mm256_set1_ps(4.0f);
    __m256 v_2_0 = _mm256_set1_ps(2.0f);
    __m256 v_3_0 = _mm256_set1_ps(3.0f);
    __m256 v_pi = _mm256_set1_ps(M_PI);
    __m256 v_phi_mul = _mm256_set1_ps(M_PI * 2.0f * 100.0f); // Wrap phi around 100 times

    __m256 v_dt = _mm256_set1_ps(dt);
    __m256 v_k = _mm256_set1_ps(4.0f * dt);
    __m256 v_damp = _mm256_set1_ps(0.92f);

    int i = 0;
    for (; i <= count - 8; i += 8) {
        __m256 v_s = _mm256_loadu_ps(&seed[i]);

        // 1. Map seed to Theta [0, PI] and Phi [0, 2PI * 100]
        __m256 v_theta = _mm256_mul_ps(v_s, v_pi);
        __m256 v_phi = _mm256_mul_ps(v_s, v_phi_mul);

        __m256 v_ny = fast_cos_avx(v_theta);
        __m256 v_sin_theta = fast_sin_avx(v_theta);

        __m256 v_nx = _mm256_mul_ps(v_sin_theta, fast_cos_avx(v_phi));
        __m256 v_nz = _mm256_mul_ps(v_sin_theta, fast_sin_avx(v_phi));

        // 2. PARADOX MATH
        __m256 v_waves = fast_cos_avx(_mm256_mul_ps(v_phi, v_4_0));
        __m256 v_twist = fast_sin_avx(_mm256_mul_ps(v_theta, v_2_0));

        __m256 v_r_corr = _mm256_mul_ps(v_base_radius,
                          _mm256_mul_ps(v_bulge,
                          _mm256_mul_ps(v_waves,
                          _mm256_mul_ps(v_twist, v_1_2))));

        __m256 v_r_main = _mm256_mul_ps(v_base_radius, v_eversion);

        // 3. APPLY DISPLACEMENT
        __m256 v_tx = _mm256_fmadd_ps(v_nx, _mm256_add_ps(v_r_main, v_r_corr), v_cx);
        __m256 v_tz = _mm256_fmadd_ps(v_nz, _mm256_add_ps(v_r_main, v_r_corr), v_cz);

        __m256 v_ty_offset = _mm256_mul_ps(fast_cos_avx(_mm256_mul_ps(v_theta, v_3_0)),
                             _mm256_mul_ps(v_base_radius,
                             _mm256_mul_ps(v_bulge, v_0_5)));

        __m256 v_ty = _mm256_add_ps(v_cy, _mm256_fmadd_ps(v_ny, v_r_main, v_ty_offset));

        // 4. SPRING PHYSICS
        __m256 v_px = _mm256_loadu_ps(&px[i]);
        __m256 v_py = _mm256_loadu_ps(&py[i]);
        __m256 v_pz = _mm256_loadu_ps(&pz[i]);

        __m256 v_vx = _mm256_loadu_ps(&vx[i]);
        __m256 v_vy = _mm256_loadu_ps(&vy[i]);
        __m256 v_vz = _mm256_loadu_ps(&vz[i]);

        v_vx = _mm256_mul_ps(_mm256_fmadd_ps(_mm256_sub_ps(v_tx, v_px), v_k, v_vx), v_damp);
        v_vy = _mm256_mul_ps(_mm256_fmadd_ps(_mm256_sub_ps(v_ty, v_py), v_k, v_vy), v_damp);
        v_vz = _mm256_mul_ps(_mm256_fmadd_ps(_mm256_sub_ps(v_tz, v_pz), v_k, v_vz), v_damp);

        v_px = _mm256_fmadd_ps(v_vx, v_dt, v_px);
        v_py = _mm256_fmadd_ps(v_vy, v_dt, v_py);
        v_pz = _mm256_fmadd_ps(v_vz, v_dt, v_pz);

        _mm256_storeu_ps(&px[i], v_px);
        _mm256_storeu_ps(&py[i], v_py);
        _mm256_storeu_ps(&pz[i], v_pz);
        _mm256_storeu_ps(&vx[i], v_vx);
        _mm256_storeu_ps(&vy[i], v_vy);
        _mm256_storeu_ps(&vz[i], v_vz);
    }
    // ========================================================
    // SCALAR TAIL LOOP (For Safety - Mod 8 remainders)
    // ========================================================
    for (; i < count; i++) {
        float s = seed[i];

        // 1. Map seed to Theta and Phi
        float theta = s * M_PI;
        float phi = s * (M_PI * 2.0f * 100.0f);

        float ny = cosf(theta);
        float sin_theta = sinf(theta);

        float nx = sin_theta * cosf(phi);
        float nz = sin_theta * sinf(phi);

        // 2. PARADOX MATH
        float waves = cosf(phi * 4.0f);
        float twist = sinf(theta * 2.0f);

        float r_corr = 4000.0f * bulge_scalar * waves * twist * 1.2f;
        float r_main = 4000.0f * eversion_scalar;

        // 3. APPLY DISPLACEMENT
        float tx = cx + nx * (r_main + r_corr);
        float tz = cz + nz * (r_main + r_corr);

        float ty_offset = cosf(theta * 3.0f) * 4000.0f * bulge_scalar * 0.5f;
        float ty = cy + (ny * r_main) + ty_offset;

        // 4. SPRING PHYSICS
        float p_x = px[i], p_y = py[i], p_z = pz[i];
        float v_x = vx[i], v_y = vy[i], v_z = vz[i];

        float k = 4.0f * dt;
        float damp = 0.92f;

        // v += (target - p) * k * dt; v *= damp;
        v_x = (v_x + (tx - p_x) * k) * damp;
        v_y = (v_y + (ty - p_y) * k) * damp;
        v_z = (v_z + (tz - p_z) * k) * damp;

        // p += v * dt;
        px[i] = p_x + v_x * dt;
        py[i] = p_y + v_y * dt;
        pz[i] = p_z + v_z * dt;

        vx[i] = v_x;
        vy[i] = v_y;
        vz[i] = v_z;
    }
}
// ============================================================================
// 4. THE FORK-JOIN THREAD POOL STATE
// ============================================================================
#define MAX_WORKERS 32

typedef enum { JOB_NONE, JOB_SWARM_STEP, JOB_EXIT } JobType;

typedef struct {
    int start_idx;
    int end_idx;

    // Arrays
    float *px, *py, *pz, *vx, *vy, *vz, *seed;

    // State variables (Passed from Lua every frame)
    float cx, cy, cz, time, dt;
    float gravity, blend_metal, blend_paradox;
    int state;
    int push_active;
    int pull_active;
} WorkerContext;

static vmath_thread_t g_threads[MAX_WORKERS];
static WorkerContext g_contexts[MAX_WORKERS];
static int g_num_workers = 0;

static vmath_mutex_t g_job_mutex;
static vmath_cond_t g_job_cond;
static vmath_cond_t g_main_cond;

static JobType g_current_job = JOB_NONE;
static int g_jobs_completed = 0;
static int g_jobs_dispatched = 0;

// ============================================================================
// 5. THE WORKER EXECUTION PIPELINE
// ============================================================================
THREAD_FUNC worker_loop(void* arg) {
    int id = (int)(intptr_t)arg;
    WorkerContext* ctx = &g_contexts[id];

    while (1) {
        // --- 1. WAIT FOR JOB BROADCAST ---
        vmath_mutex_lock(&g_job_mutex);
        while (g_current_job == JOB_NONE) {
            vmath_cond_wait(&g_job_cond, &g_job_mutex);
        }
        JobType job = g_current_job;
        vmath_mutex_unlock(&g_job_mutex);

        if (job == JOB_EXIT) break;

        // --- 2. EXECUTE KERNEL PIPELINE ---
        if (job == JOB_SWARM_STEP) {
            int chunk = ctx->end_idx - ctx->start_idx;
            int start = ctx->start_idx;

            // Phase A: Physics Integration & Bounds
            // Note: Passing the same pointers for IN and OUT arrays
            vmath_swarm_update_velocities(chunk,
                ctx->px + start, ctx->py + start, ctx->pz + start,
                ctx->vx + start, ctx->vy + start, ctx->vz + start,
                ctx->px + start, ctx->py + start, ctx->pz + start,
                ctx->vx + start, ctx->vy + start, ctx->vz + start,
                -15000*32, 15000*32, -4000*32, 15000*32, -15000*32, 15000*32,
                ctx->dt, ctx->gravity);

            // Phase B: Explosions
            if (ctx->push_active) {
                vmath_swarm_apply_explosion(chunk,
                    ctx->px + start, ctx->py + start, ctx->pz + start,
                    ctx->vx + start, ctx->vy + start, ctx->vz + start,
                    0, 5000, 0, 5000000.0f * ctx->dt, 15000.0f);
            }
            if (ctx->pull_active) {
                vmath_swarm_apply_explosion(chunk,
                    ctx->px + start, ctx->py + start, ctx->pz + start,
                    ctx->vx + start, ctx->vy + start, ctx->vz + start,
                    0, 5000, 0, -4000000.0f * ctx->dt, 20000.0f);
            }

            // Phase C: Shape Evaluation
            switch (ctx->state) {
                case 1:
                    vmath_swarm_bundle(chunk, ctx->px + start, ctx->py + start, ctx->pz + start, ctx->vx + start, ctx->vy + start, ctx->vz + start, ctx->seed + start, ctx->cx, ctx->cy, ctx->cz, ctx->time, ctx->dt);
                    break;
                case 2:
                    vmath_swarm_galaxy(chunk, ctx->px + start, ctx->py + start, ctx->pz + start, ctx->vx + start, ctx->vy + start, ctx->vz + start, ctx->seed + start, ctx->cx, ctx->cy, ctx->cz, ctx->time, ctx->dt);
                    break;
                case 3:
                    vmath_swarm_tornado(chunk, ctx->px + start, ctx->py + start, ctx->pz + start, ctx->vx + start, ctx->vy + start, ctx->vz + start, ctx->seed + start, ctx->cx, ctx->cy, ctx->cz, ctx->time, ctx->dt);
                    break;
                case 4:
                    vmath_swarm_gyroscope(chunk, ctx->px + start, ctx->py + start, ctx->pz + start, ctx->vx + start, ctx->vy + start, ctx->vz + start, ctx->seed + start, ctx->cx, ctx->cy, ctx->cz, ctx->time, ctx->dt);
                    break;
                case 5:
                    vmath_swarm_metal(chunk, ctx->px + start, ctx->py + start, ctx->pz + start, ctx->vx + start, ctx->vy + start, ctx->vz + start, ctx->seed + start, ctx->cx, ctx->cy, ctx->cz, ctx->time, ctx->dt, ctx->blend_metal);
                    break;
                case 6:
                    vmath_swarm_smales(chunk, ctx->px + start, ctx->py + start, ctx->pz + start, ctx->vx + start, ctx->vy + start, ctx->vz + start, ctx->seed + start, ctx->cx, ctx->cy, ctx->cz, ctx->time, ctx->dt, ctx->blend_paradox);
                    break;
            }
        }

        // --- 3. REPORT COMPLETION ---
        vmath_mutex_lock(&g_job_mutex);
        g_jobs_completed++;
        if (g_jobs_completed == g_jobs_dispatched) {
            vmath_cond_broadcast(&g_main_cond); // Wake the main Lua thread
        }
        while (g_current_job != JOB_NONE) {
            vmath_cond_wait(&g_job_cond, &g_job_mutex); // Sleep until job state resets
        }
        vmath_mutex_unlock(&g_job_mutex);
    }
    return THREAD_RETURN_VAL;
}

// 6. LUA FFI BRIDGE (The C API)
EXPORT void vmath_init_workers(int num_threads) {
    g_num_workers = (num_threads > MAX_WORKERS) ? MAX_WORKERS : num_threads;
    vmath_mutex_init(&g_job_mutex);
    vmath_cond_init(&g_job_cond);
    vmath_cond_init(&g_main_cond);

    for (int i = 0; i < g_num_workers; i++) {
        g_threads[i] = vmath_thread_start(worker_loop, (void*)(intptr_t)i);
    }
}

EXPORT void vmath_destroy_workers() {
    vmath_mutex_lock(&g_job_mutex);
    g_current_job = JOB_EXIT;
    vmath_cond_broadcast(&g_job_cond);
    vmath_mutex_unlock(&g_job_mutex);

    for (int i = 0; i < g_num_workers; i++) {
        vmath_thread_join(g_threads[i]);
    }

    vmath_mutex_destroy(&g_job_mutex);
    vmath_cond_destroy(&g_job_cond);
    vmath_cond_destroy(&g_main_cond);
}

// Replaces vmath_step_swarm
EXPORT void vmath_dispatch_swarm(
    int count,
    float* px, float* py, float* pz,
    float* vx, float* vy, float* vz,
    float* seed,
    int state, int push, int pull,
    float cx, float cy, float cz,
    float time, float dt, float gravity,
    float blend_metal, float blend_paradox)
{
    if (g_num_workers == 0) return;

    int chunk_size = count / g_num_workers;

    vmath_mutex_lock(&g_job_mutex);
    g_jobs_completed = 0;
    g_jobs_dispatched = g_num_workers;

    for (int i = 0; i < g_num_workers; i++) {
        g_contexts[i].start_idx = i * chunk_size;
        g_contexts[i].end_idx = (i == g_num_workers - 1) ? count : (i + 1) * chunk_size;

        // Map arrays
        g_contexts[i].px = px; g_contexts[i].py = py; g_contexts[i].pz = pz;
        g_contexts[i].vx = vx; g_contexts[i].vy = vy; g_contexts[i].vz = vz;
        g_contexts[i].seed = seed;

        // Map constants
        g_contexts[i].state = state;
        g_contexts[i].push_active = push;
        g_contexts[i].pull_active = pull;
        g_contexts[i].cx = cx; g_contexts[i].cy = cy; g_contexts[i].cz = cz;
        g_contexts[i].time = time;
        g_contexts[i].dt = dt;
        g_contexts[i].gravity = gravity;
        g_contexts[i].blend_metal = blend_metal;
        g_contexts[i].blend_paradox = blend_paradox;
    }

    // Fire the broadcast
    g_current_job = JOB_SWARM_STEP;
    vmath_cond_broadcast(&g_job_cond);

    // Block Lua until frame is resolved
    while (g_jobs_completed < g_num_workers) {
        vmath_cond_wait(&g_main_cond, &g_job_mutex);
    }

    // Acknowledge completion and reset
    g_current_job = JOB_NONE;
    vmath_cond_broadcast(&g_job_cond);
    vmath_mutex_unlock(&g_job_mutex);
}

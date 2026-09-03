// Minimal cross-platform dynamic-library shim so the NVIDIA encoder/decoder
// factory probes build on Windows (MSVC) as well as Linux. Added for the
// windows-nvenc fork — see webrtc-sys/build.rs.
#ifndef WEBRTC_SYS_NVIDIA_DL_COMPAT_H
#define WEBRTC_SYS_NVIDIA_DL_COMPAT_H

#if defined(_WIN32)
#include <windows.h>
static inline void* lk_dlopen(const char* name) {
  return reinterpret_cast<void*>(::LoadLibraryA(name));
}
static inline void* lk_dlsym(void* handle, const char* symbol) {
  return reinterpret_cast<void*>(
      ::GetProcAddress(reinterpret_cast<HMODULE>(handle), symbol));
}
static inline void lk_dlclose(void* handle) {
  ::FreeLibrary(reinterpret_cast<HMODULE>(handle));
}
#define LK_NVENC_RUNTIME_LIB "nvEncodeAPI64.dll"
#define LK_NVDEC_RUNTIME_LIB "nvcuvid.dll"
#else
#include <dlfcn.h>
static inline void* lk_dlopen(const char* name) {
  return dlopen(name, RTLD_LAZY);
}
static inline void* lk_dlsym(void* handle, const char* symbol) {
  return dlsym(handle, symbol);
}
static inline void lk_dlclose(void* handle) { dlclose(handle); }
#define LK_NVENC_RUNTIME_LIB "libnvidia-encode.so.1"
#define LK_NVDEC_RUNTIME_LIB "libnvcuvid.so.1"
#endif

#endif  // WEBRTC_SYS_NVIDIA_DL_COMPAT_H

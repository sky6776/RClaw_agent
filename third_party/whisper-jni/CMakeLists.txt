# RClaw whisper JNI：whisper.cpp 静态链接进单个 librclaw_whisper.so（含 ggml）。
cmake_minimum_required(VERSION 3.14)
project(rclaw_whisper_jni C CXX)

set(WHISPER_BUILD_EXAMPLES OFF CACHE BOOL "" FORCE)
set(WHISPER_BUILD_TESTS OFF CACHE BOOL "" FORCE)
set(GGML_OPENMP OFF CACHE BOOL "" FORCE)   # bionic 无 OpenMP
set(BUILD_SHARED_LIBS OFF CACHE BOOL "" FORCE)

add_subdirectory(${CMAKE_CURRENT_SOURCE_DIR}/../whisper.cpp whisper.cpp)

add_library(rclaw_whisper SHARED whisper_jni.cpp)
target_link_libraries(rclaw_whisper PRIVATE whisper)
target_include_directories(rclaw_whisper PRIVATE
        ${CMAKE_CURRENT_SOURCE_DIR}/../whisper.cpp/include)
target_compile_features(rclaw_whisper PRIVATE cxx_std_17)

cmake_minimum_required(VERSION 3.10)
project(musegadget_tests C)

# Host-only test drivers for the library in ../muse. Build:
#   cmake -S tests -B build-host && cmake --build build-host
# Sanitizers: add -DMUSE_SANITIZE=ON.

add_subdirectory(${CMAKE_CURRENT_SOURCE_DIR}/../muse ${CMAKE_BINARY_DIR}/muse)

add_executable(host_session host_session.c)
target_link_libraries(host_session PRIVATE musegadget)
set_target_properties(host_session PROPERTIES C_STANDARD 99 C_STANDARD_REQUIRED ON C_EXTENSIONS OFF)
target_compile_options(host_session PRIVATE -Wall -Wextra -Werror)

add_executable(tls_check tls_check.c)
target_link_libraries(tls_check PRIVATE musegadget)
set_target_properties(tls_check PROPERTIES C_STANDARD 99 C_STANDARD_REQUIRED ON C_EXTENSIONS OFF)
target_compile_options(tls_check PRIVATE -Wall -Wextra -Werror)

add_executable(live_check live_check.c)
target_link_libraries(live_check PRIVATE musegadget)
set_target_properties(live_check PROPERTIES C_STANDARD 99 C_STANDARD_REQUIRED ON C_EXTENSIONS OFF)
target_compile_options(live_check PRIVATE -Wall -Wextra -Werror)

add_executable(speech_check speech_check.c)
target_link_libraries(speech_check PRIVATE musegadget)
set_target_properties(speech_check PROPERTIES C_STANDARD 99 C_STANDARD_REQUIRED ON)
target_compile_options(speech_check PRIVATE -Wall -Wextra -Werror)

add_executable(dns_check dns_check.c)
target_link_libraries(dns_check PRIVATE musegadget)
target_compile_options(dns_check PRIVATE -Wall -Wextra -Werror)

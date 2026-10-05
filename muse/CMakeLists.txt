cmake_minimum_required(VERSION 3.10)
project(musegadget C)

option(MUSE_SANITIZE "Build with AddressSanitizer and UndefinedBehaviorSanitizer" OFF)

set(MUSE_SOURCES
    buf.c url.c log.c tls.c root_cert.c stream.c http.c ws.c
    proto.c noise.c api.c link.c service.c dns.c)

if(PSP)
    list(APPEND MUSE_SOURCES port_psp.c port_sdl2_sync.c)
else()
    list(APPEND MUSE_SOURCES port_posix.c)
endif()

add_library(musegadget STATIC ${MUSE_SOURCES})
target_include_directories(musegadget PUBLIC ${CMAKE_CURRENT_SOURCE_DIR})
set_target_properties(musegadget PROPERTIES C_STANDARD 99 C_STANDARD_REQUIRED ON C_EXTENSIONS OFF)
target_compile_options(musegadget PRIVATE -Wall -Wextra -Wpedantic -Wshadow -Wconversion -Wvla -Werror)

if(PSP)
    # pspdev.cmake already adds psp/include and psp/sdk/include. The PSP
    # port compiles the SDL2 sync shim against the toolchain's SDL2.
    find_package(SDL2 REQUIRED)
    target_include_directories(musegadget PRIVATE ${SDL2_INCLUDE_DIRS})
    target_link_libraries(musegadget PUBLIC mbedtls mbedx509 mbedcrypto cjson
        pspnet_inet pspnet_resolver pspnet_apctl pspnet psprtc pspopenpsid)
else()
    # Host: brew mbedtls@2 (2.28, the PSP's version) and brew cjson.
    find_library(MBEDTLS_LIB mbedtls PATHS /opt/homebrew/opt/mbedtls@2/lib /usr/local/opt/mbedtls@2/lib NO_DEFAULT_PATH REQUIRED)
    find_library(MBEDX509_LIB mbedx509 PATHS /opt/homebrew/opt/mbedtls@2/lib /usr/local/opt/mbedtls@2/lib NO_DEFAULT_PATH REQUIRED)
    find_library(MBEDCRYPTO_LIB mbedcrypto PATHS /opt/homebrew/opt/mbedtls@2/lib /usr/local/opt/mbedtls@2/lib NO_DEFAULT_PATH REQUIRED)
    find_path(MBEDTLS_INCLUDE mbedtls/ssl.h PATHS /opt/homebrew/opt/mbedtls@2/include /usr/local/opt/mbedtls@2/include NO_DEFAULT_PATH REQUIRED)
    find_library(CJSON_LIB cjson PATHS /opt/homebrew/opt/cjson/lib /usr/local/opt/cjson/lib REQUIRED)
    find_path(CJSON_INCLUDE cjson/cJSON.h PATHS /opt/homebrew/opt/cjson/include /usr/local/opt/cjson/include REQUIRED)
    target_include_directories(musegadget PUBLIC ${MBEDTLS_INCLUDE} ${CJSON_INCLUDE})
    target_link_libraries(musegadget PUBLIC ${MBEDTLS_LIB} ${MBEDX509_LIB} ${MBEDCRYPTO_LIB} ${CJSON_LIB} pthread)
    if(MUSE_SANITIZE)
        target_compile_options(musegadget PUBLIC -fsanitize=address,undefined -fno-omit-frame-pointer -g)
        target_link_options(musegadget PUBLIC -fsanitize=address,undefined)
    endif()
endif()

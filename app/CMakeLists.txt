cmake_minimum_required(VERSION 3.10)
project(muse C)

add_subdirectory(${CMAKE_CURRENT_SOURCE_DIR}/../muse ${CMAKE_BINARY_DIR}/musegadget)

# The Pico speech engine, as its authors wrote it, so its warnings stay off.
file(GLOB PICO_SOURCES ${CMAKE_CURRENT_SOURCE_DIR}/../third_party/pico/*.c)
add_library(pico STATIC ${PICO_SOURCES})
target_include_directories(pico PUBLIC ${CMAKE_CURRENT_SOURCE_DIR}/../third_party/pico)
target_compile_options(pico PRIVATE -w -O2)

add_executable(${PROJECT_NAME} main.c link.c native_tts.c)

find_package(SDL2 REQUIRED)

target_include_directories(${PROJECT_NAME} PRIVATE ${SDL2_INCLUDE_DIRS})
target_link_libraries(${PROJECT_NAME} PRIVATE musegadget pico
    SDL2_image SDL2_ttf harfbuzz freetype png16 jpeg bz2 z m
    ${SDL2_LIBRARIES} psputility pspaudio psppower)

if(PSP)
    create_pbp_file(
        TARGET ${PROJECT_NAME}
        ICON_PATH ${CMAKE_CURRENT_SOURCE_DIR}/../assets/menu/ICON0.PNG
        BACKGROUND_PATH ${CMAKE_CURRENT_SOURCE_DIR}/../assets/menu/PIC1.PNG
        PREVIEW_PATH NULL
        TITLE "Muse"
        # 1 asks for all of the PSP's memory. The toolchain's default of 2
        # leaves the app the 24 MB of the first PSP model, which the avatar's
        # frames and the speech engine together do not fit in.
        MEMSIZE 1
    )
endif()

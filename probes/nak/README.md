# NAK sobre musl estático (medición de la puerta G3)

`probe.c` compila un shader de cómputo con NAK para SM86 (GA106) y muestra tamaño, huella y desensamblado. Se construye dos veces
(glibc nativo, para depurar la sonda; musl estático, el que corre en constanos) y las dos salidas deben ser idénticas.
Resultado y conclusiones: `docs/gpu/phase7-3d-decision.md`, sección "Resultados de G3".

Receta (todo fuera del repo, en `~/src/gpu-ref/`):

1. Mesa `main` 20f48abe en `~/src/gpu-ref/mesa`; el clon *sparse* necesita `git sparse-checkout add src subprojects bin`.
2. Paquetes: `spirv-llvm-translator` (pacman), y con cargo `bindgen-cli` y `cbindgen`. Ya estaban: `musl`, `python-mako`, `python-yaml`, `libdrm`, `llvm`.
3. Cabeceras de terceros para musl en `~/src/gpu-ref/musl-inc/` (enlaces simbólicos a `/usr/include/{linux,asm,asm-generic,drm,libdrm,
   xf86drm.h,xf86drmMode.h,libudev.h,libelf.h,gelf.h,nlist.h,spirv-tools,libdisplay-info}`) más `musl_compat.h` (declara
   `pthread_mutex_clocklock` y `pthread_cond_clockwait`, que las cabeceras de libstdc++ de glibc usan y musl no declara).
4. Herramientas nativas de build: en `build-nak` (Meson nativo, `-Dllvm=enabled -Dshared-llvm=enabled`) construir
   `src/compiler/clc/mesa_clc` y `src/compiler/spirv/vtn_bindgen2`, y copiarlas a `~/src/gpu-ref/native-tools/`.
5. Meson cruzado: `PATH=~/src/gpu-ref/native-tools:$PATH meson setup build-musl --cross-file mesa-port/musl-cross.ini
   --default-library=static -Dvulkan-drivers=nouveau -Dgallium-drivers= -Dplatforms= -Dllvm=disabled -Dopengl=false -Dgles1=disabled
   -Dgles2=disabled -Degl=disabled -Dgbm=disabled -Dglx=disabled -Dvideo-codecs= -Dvulkan-layers= -Dbuild-tests=false -Dtools=
   -Dlibunwind=disabled -Dlmsensors=disabled -Dxlib-lease=disabled -Dzlib=disabled -Dzstd=disabled -Dexpat=disabled
   -Dshader-cache=disabled -Dmesa-clc=system -Dprecomp-compiler=system`, y `ninja -k 0` (27 s; solo falla el enlace de
   `libvulkan_nouveau.so`, que intenta enlazar las `.so` de glibc).
6. `python3 probes/nak/build.py native|musl` (enlaza `libnak.a`, `libnak_rs.a`, NIR y util; para musl, `-nostdlib` con `crt1.o`, `libc.a` y
   `libm.a` de musl, `libstdc++.a` de gcc, `libgcc.a` y el `libunwind.a` de Rust).
7. Correrlo en constanos: `strip`, `debugfs -w -R "write nak-probe-musl.stripped /nak-probe" disk.img`, `/mnt/nak-probe` en el shell.

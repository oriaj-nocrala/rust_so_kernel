# 🦀 SO2 - Rust Operating System Kernel

Un proyecto de aprendizaje para desarrollar un kernel de sistema operativo básico en Rust, enfocado en arquitectura x86_64.

## 📋 Descripción

Este proyecto es un kernel de sistema operativo simple escrito en Rust que incluye funcionalidades básicas como:

- ⌨️ **Manejo de teclado**: Procesamiento de scancodes y eventos de teclado
- 🖥️ **Framebuffer**: Gestión básica de gráficos
- ⚡ **Interrupciones**: Sistema de manejo de interrupciones con IDT
- ⏰ **Timer (PIT)**: Programmable Interval Timer para temporización
- 🔧 **Arquitectura x86_64**: Diseñado específicamente para procesadores de 64 bits

## 🏗️ Arquitectura del Proyecto

```
so2/
├── kernel/          # Código del kernel principal
│   ├── src/
│   │   ├── main.rs          # Punto de entrada del kernel
│   │   ├── framebuffer.rs   # Gestión del framebuffer
│   │   ├── keyboard.rs      # Driver del teclado
│   │   ├── pit.rs          # Programmable Interval Timer
│   │   └── interrupts/     # Sistema de interrupciones
│   │       ├── mod.rs
│   │       ├── idt.rs      # Interrupt Descriptor Table
│   │       └── pic.rs      # Programmable Interrupt Controller
│   └── tests/       # Tests del kernel
├── src/
│   └── main.rs      # Bootloader principal
└── x86_64-os.json   # Configuración del target personalizado
```

## 🚀 Características

- **No Standard Library** (`#![no_std]`): Funcionamiento en bare metal
- **Bootloader personalizado**: Usando `bootloader-api 0.11`
- **Interrupciones x86**: Implementación del trait `x86-interrupt`
- **Gestión de memoria**: Configuración básica para entorno sin OS
- **Tests integrados**: Framework de testing para el kernel

## 🛠️ Dependencias Principales

- `bootloader 0.9` + `bootloader_api 0.11.10`
- `x86_64 0.15.2` - Abstracciones para arquitectura x86_64
- `lazy_static` - Inicialización estática lazy
- `font8x8` - Fuentes para el framebuffer
- `linked_list_allocator` - Allocador de memoria

## 📚 Propósito Educativo

Este proyecto está diseñado como una herramienta de aprendizaje para entender:

- Programación de sistemas de bajo nivel
- Arquitectura de sistemas operativos
- Manejo de hardware en Rust
- Desarrollo en bare metal
- Interrupciones y manejo de eventos

## 🎯 Estado del Proyecto

🚧 **En desarrollo activo** - Proyecto de aprendizaje en progreso

### Implementado:
- ✅ Sistema básico de interrupciones
- ✅ Driver de teclado funcional
- ✅ Framebuffer básico
- ✅ Timer PIT
- ✅ Bootloader personalizado

### Por implementar:
- ⏳ Gestión avanzada de memoria
- ⏳ Sistema de archivos básico
- ⏳ Multitasking
- ⏳ Drivers adicionales

---

*Este es un proyecto de aprendizaje personal para explorar el desarrollo de sistemas operativos con Rust.*
//! SCD scripts: opcode tables, reader, IR, disassembler, decompiler and VMs.
//!
//! An RDT stores three kinds of script data: the `init` procedure container,
//! the `main` procedure container (both run by the command VM) and a table of
//! event scripts (run by the event VM). The reader decodes the raw bytes into
//! an instruction IR that keeps every original byte and absolute file offset.

pub mod decomp;
pub mod disasm;
pub mod host;
pub mod ir;
pub mod opcode;
pub mod reader;
pub mod vm;

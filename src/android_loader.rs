use crate::android_library::{AndroidLibrary, Symbol};
use crate::hook_manager;
use crate::sysv64;
use anyhow::Result;
use elfloader::arch::{aarch64, arm, x86, x86_64};
use elfloader::{
    ElfBinary, ElfLoader, ElfLoaderErr, LoadableHeaders, RelocationEntry, RelocationType,
};
use memmap2::MmapOptions;
use region::Protection;
use std::cmp::max;
use std::collections::HashMap;
use std::ffi::CStr;
use std::fs;
use std::os::raw::{c_char, c_void};
use std::path::PathBuf;
use std::ptr::null_mut;
use xmas_elf::program::{ProgramHeader, Type};
use xmas_elf::sections::SectionData;
use xmas_elf::symbol_table::Entry;

pub struct AndroidLoader {}

impl AndroidLoader {
    #[sysv64]
    fn pthread_stub() -> i32 {
        0
    }

    #[sysv64]
    fn undefined_symbol_stub() {
        panic!("tried to call an undefined symbol");
    }

    #[sysv64]
    unsafe fn dlopen(name: *const c_char) -> *mut c_void {
        use crate::hook_manager::get_hooks;
        let mut path_str = CStr::from_ptr(name).to_str().unwrap();

        #[cfg(target_family = "windows")]
        {
            path_str = path_str.chars()
                .map(|x| match x {
                    '\\' => '/',
                    c => c
                }).collect::<String>();

            path_str = path_str.as_str();
        }

        println!("Loading {}", path_str);
        match Self::load_library(path_str) {
            Ok(lib) => Box::into_raw(Box::new(lib)) as *mut c_void,
            Err(_) => null_mut(),
        }
    }

    #[sysv64]
    unsafe fn dlsym(library: *mut AndroidLibrary, symbol: *const c_char) -> *mut c_void {
        let symbol = CStr::from_ptr(symbol).to_str().unwrap();
        println!("Symbol requested: {}", symbol);
        match library.as_ref().and_then(|lib| lib.get_symbol(symbol)) {
            Some(func) => func as *mut c_void,
            None => null_mut(),
        }
    }

    #[sysv64]
    unsafe fn dlclose(library: *mut AndroidLibrary) {
        let _ = Box::from_raw(library);
    }

    fn symbol_finder(symbol_name: &str, library: &AndroidLibrary, hooks: &HashMap<String, usize>) -> *const () {
        // Check if this function is hooked for this library

        if let Some(func) = hooks.get(symbol_name) {
            *func as *const ()
        // pthread functions are problematic, let's ignore them
        } else {
            Self::get_libc_symbol(symbol_name)
        }
    }

    fn get_libc_symbol(symbol_name: &str) -> *const () {
        if symbol_name.starts_with("pthread_") {
            Self::pthread_stub as *const ()
        } else {
            match symbol_name {
                "dlopen" => Self::dlopen as *const (),
                "dlsym" => Self::dlsym as *const (),
                "dlclose" => Self::dlclose as *const (),
                _ => Self::undefined_symbol_stub as *const ()
            }
        }
    }

    pub fn load_library(path: &str) -> Result<AndroidLibrary> {
        let file = fs::read(path)?;
        let bin = ElfBinary::new(file.as_slice())?;

        Ok(bin.load::<Self, AndroidLibrary>()?)
    }
}

impl AndroidLoader {
    fn absolute_reloc(library: &mut AndroidLibrary, hooks: &HashMap<String, usize>, entry: &RelocationEntry, addend: usize) {
        let name = &library.strings.get(&(entry.index as usize));
        let symbol = Self::symbol_finder(name.unwrap(), library, hooks);

        // addend is always 0, but we still add it to be safe
        // converted to an array in the systme endianess
        let relocated = addend.wrapping_add(symbol as usize).to_ne_bytes();

        let offset = entry.offset as usize;
        library.memory_map[offset..offset + relocated.len()].copy_from_slice(&relocated);
    }

    fn relative_reloc(library: &mut AndroidLibrary, entry: &RelocationEntry, addend: usize) {
        let relocated = addend
            .wrapping_add(library.memory_map.as_mut_ptr() as usize)
            .to_ne_bytes();

        let offset = entry.offset as usize;
        library.memory_map[offset..offset + relocated.len()].copy_from_slice(&relocated);
    }

    #[cfg(not(target_arch="aarch64"))]
    const MAX_PAGE_SIZE: usize = 4096;

    #[cfg(target_arch="aarch64")]
    const MAX_PAGE_SIZE: usize = 65536;
}

impl ElfLoader<AndroidLibrary> for AndroidLoader {
    fn allocate(
        load_headers: LoadableHeaders,
        elf_binary: &ElfBinary
    ) -> Result<AndroidLibrary, ElfLoaderErr> {
        let mut minimum = usize::MAX;
        let mut maximum = usize::MIN;

        for header in load_headers {
            if header.get_type() == Ok(Type::Load) {
                let start = region::page::floor(header.virtual_addr() as *const ()) as usize;
                let end = region::page::ceil(
                    (start as usize + max(header.file_size(), header.mem_size()) as usize)
                        as *const (),
                ) as usize;

                if start < minimum {
                    minimum = start;
                }

                if end > maximum {
                    maximum = end;
                }
            }
        }

        let alloc_start = region::page::floor(minimum as *const ()) as usize;
        debug_assert!(alloc_start <= minimum);
        let alloc_end = region::page::ceil(maximum as *const ()) as usize;
        debug_assert!(alloc_end >= maximum);

        let mut dyn_symbol_section = None;
        let mut gnu_hash_section = None;

        elf_binary
            .file
            .section_iter()
            .for_each(|elem| {
           match elem.get_name(&elf_binary.file) {
               Ok(".dynsym") => {
                   dyn_symbol_section = Some(elem);
               }
               Ok(".gnu.hash") => {
                   gnu_hash_section = Some(elem);
               }
               _ => {}
           }
        });

        let dyn_symbol_table = dyn_symbol_section.unwrap().get_data(&elf_binary.file).unwrap();

        let mut symbols = HashMap::new();
        let mut strings = HashMap::new();

        let mut i = 0;

        match dyn_symbol_table { // FIXME expensive
            SectionData::DynSymbolTable64(entries) => entries
                .iter()
                .for_each(|s| {
                    let name = elf_binary.symbol_name(s).to_string();
                    symbols.insert(
                        name.clone(),
                        Symbol {
                            name: name.clone(),
                            value: s.value() as usize
                        }
                    );
                    strings.insert(i as usize, name);
                    i += 1;
                }),
            SectionData::DynSymbolTable32(entries) => entries
                .iter()
                .for_each(|s| {
                    let name = elf_binary.symbol_name(s).to_string();
                    symbols.insert(
                        name.clone(),
                        Symbol {
                            name: name.clone(),
                            value: s.value() as usize
                        }
                    );
                    strings.insert(i, name);
                    i += 1;
                }),
            _ => { }
        };

        if let Ok(map) = MmapOptions::new().len(alloc_end - alloc_start).map_anon() {
            Ok(AndroidLibrary {
                memory_map: map,
                symbols,
                strings
            })
        } else {
            Err(ElfLoaderErr::ElfParser {
                source: "Memory mapping failed!",
            })
        }
    }

    fn load(
        library: &mut AndroidLibrary,
        program_header: &ProgramHeader,
        region: &[u8],
    ) -> Result<(), ElfLoaderErr> {
        let virtual_addr = program_header.virtual_addr() as usize;
        let mem_size = program_header.mem_size() as usize;
        let file_size = program_header.file_size() as usize;
        let addr = library.memory_map.as_ptr() as usize;

        let start_addr = region::page::floor((addr + virtual_addr) as *const c_void) as *mut c_void;
        let end_addr = region::page::ceil((addr + virtual_addr + mem_size) as *const c_void);
        print!(
            "{:x} - {:x} (mem_sz: {}, file_sz: {}) [",
            start_addr as usize, end_addr as usize, mem_size, file_size
        );

        let is_standard_page = region::page::size() <= Self::MAX_PAGE_SIZE;

        let flags = program_header.flags();
        let mut prot = Protection::NONE.bits();
        if flags.is_read() || !is_standard_page {
            print!("R");
            prot |= Protection::READ.bits();
        } else {
            print!("-");
        }
        if flags.is_write() || !is_standard_page {
            print!("W");
            prot |= Protection::WRITE.bits();
        } else {
            print!("-");
        }
        if flags.is_execute() || !is_standard_page {
            println!("X]");
            prot |= Protection::EXECUTE.bits();
        } else {
            println!("-]");
        }
        library.memory_map[virtual_addr..virtual_addr + file_size].copy_from_slice(region);

        unsafe {
            region::protect(
                start_addr,
                end_addr as usize - start_addr as usize,
                Protection::from_bits_truncate(prot),
            )
            .unwrap()
        };

        Ok(())
    }

    fn relocate(library: &mut AndroidLibrary, entries: Vec<RelocationEntry>) -> Result<(), ElfLoaderErr> {
        use crate::hook_manager::get_hooks;

        let hooks = get_hooks();

        for entry in entries.iter() {
            match entry.rtype {
                RelocationType::x86(relocation) => {
                    let addend = usize::from_ne_bytes(
                        library.memory_map[entry.offset as usize
                            ..entry.offset as usize + std::mem::size_of::<usize>()]
                            .try_into()
                            .unwrap(),
                    );
                    match relocation {
                        x86::RelocationTypes::R_386_GLOB_DAT | x86::RelocationTypes::R_386_JMP_SLOT => {
                            Self::absolute_reloc(library, &hooks, entry, 0);
                        }

                        x86::RelocationTypes::R_386_RELATIVE => {
                            Self::relative_reloc(library, entry, addend);
                        }

                        x86::RelocationTypes::R_386_32 => {
                            Self::absolute_reloc(library, &hooks, entry, addend);
                        }

                        _ => {
                            eprintln!("Unhandled relocation: {:?}", relocation);
                            return Err(ElfLoaderErr::UnsupportedRelocationEntry);
                        }
                    }
                }

                RelocationType::x86_64(relocation) => {
                    let addend = entry
                        .addend
                        .ok_or(ElfLoaderErr::UnsupportedRelocationEntry)?
                        as usize;
                    match relocation {
                        x86_64::RelocationTypes::R_AMD64_JMP_SLOT
                        | x86_64::RelocationTypes::R_AMD64_GLOB_DAT
                        | x86_64::RelocationTypes::R_AMD64_64 => {
                            Self::absolute_reloc(library, &hooks, entry, addend);
                        }

                        x86_64::RelocationTypes::R_AMD64_RELATIVE => {
                            Self::relative_reloc(library, entry, addend);
                        }

                        _ => {
                            eprintln!("Unhandled relocation: {:?}", relocation);
                            return Err(ElfLoaderErr::UnsupportedRelocationEntry);
                        }
                    }
                }

                RelocationType::Arm(relocation) => {
                    let addend = usize::from_ne_bytes(
                        library.memory_map[entry.offset as usize
                            ..entry.offset as usize + std::mem::size_of::<usize>()]
                            .try_into()
                            .unwrap(),
                    );
                    match relocation {
                        arm::RelocationTypes::R_ARM_GLOB_DAT
                        | arm::RelocationTypes::R_ARM_JUMP_SLOT => {
                            Self::absolute_reloc(library, &hooks, entry, 0);
                        }

                        arm::RelocationTypes::R_ARM_RELATIVE => {
                            Self::relative_reloc(library, entry, addend);
                        }

                        arm::RelocationTypes::R_ARM_ABS32 => {
                            Self::absolute_reloc(library, &hooks, entry, addend);
                        }

                        _ => {
                            eprintln!("Unhandled relocation: {:?}", relocation);
                            return Err(ElfLoaderErr::UnsupportedRelocationEntry);
                        }
                    }
                }

                RelocationType::AArch64(relocation) => {
                    let addend = entry
                        .addend
                        .ok_or(ElfLoaderErr::UnsupportedRelocationEntry)?
                        as usize;
                    match relocation {
                        aarch64::RelocationTypes::R_AARCH64_JUMP_SLOT
                        | aarch64::RelocationTypes::R_AARCH64_GLOB_DAT
                        | aarch64::RelocationTypes::R_AARCH64_ABS64 => {
                            Self::absolute_reloc(library, &hooks, entry, addend);
                        }

                        aarch64::RelocationTypes::R_AARCH64_RELATIVE => {
                            Self::relative_reloc(library, entry, addend);
                        }

                        _ => {
                            eprintln!("Unhandled relocation: {:?}", relocation);
                            return Err(ElfLoaderErr::UnsupportedRelocationEntry);
                        }
                    }
                }
            }
        }

        Ok(())
    }
}

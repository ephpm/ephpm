//! `[php] extensions` → generated php.ini load lines.
//!
//! PHP has two load directives, and picking the wrong one breaks the library:
//!
//! - `extension=` loads a plain module (`get_module`).
//! - `zend_extension=` loads a **Zend extension** (`zend_extension_entry`) —
//!   Xdebug, OPcache, and other engine hooks. Xdebug also exports
//!   `get_module`, so `extension=xdebug.so` "loads", but then refuses to work
//!   ("Xdebug MUST be loaded as a Zend extension"). Loaded through
//!   `zend_extension=`, Xdebug registers its own module from its Zend
//!   startup hook, so it appears as both.
//!
//! So each entry is classified by inspecting the shared object: if it
//! exports both symbols `zend_load_extension` requires —
//! `zend_extension_entry` and `extension_version_info` — it is emitted as
//! `zend_extension=`, otherwise as `extension=` exactly as before. To force
//! `extension=` for such a library, write the line yourself in
//! `[php] ini_overrides` instead of listing it here.
//!
//! Every emitted value is double-quoted with `\`, `"` and `$` escaped, so a
//! path (including the working directory a relative Zend extension path is
//! joined to) can never break PHP's ini parser. A parse error there would
//! make PHP stop reading the generated ini and silently drop the hardening
//! lines that follow it.
//!
//! `zend_extension=` also resolves paths differently. php-src's
//! `php_load_zend_extension_cb` treats any non-absolute value as relative to
//! `extension_dir` — `zend_extension=exts/xdebug.so` looks for
//! `<extension_dir>/exts/xdebug.so`, not `./exts/xdebug.so` — so a relative
//! path entry that turns out to be a Zend extension is made absolute against
//! the working directory (the same base `extension=` uses for a relative
//! `dlopen`). Bare names keep PHP's own `extension_dir` search for both
//! directives.

use std::fmt;
use std::io::Read;
use std::path::{Path, PathBuf};

/// The symbol a Zend extension exports (`ZEND_DLEXPORT zend_extension
/// zend_extension_entry`), looked up by `zend_load_extension`.
const ZEND_EXTENSION_SYMBOL: &str = "zend_extension_entry";

/// The version record `zend_load_extension` requires beside
/// [`ZEND_EXTENSION_SYMBOL`]; a library without it is not a loadable Zend
/// extension.
const ZEND_EXTENSION_VERSION_SYMBOL: &str = "extension_version_info";

/// Upper bound on how much of a candidate library is read for symbol
/// inspection. Real extensions are a few MB (mongodb ≈ 12 MB); anything far
/// past that is not an extension and is left for PHP to reject.
const MAX_INSPECT_BYTES: u64 = 256 * 1024 * 1024;

/// Which php.ini directive loads a library.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LoadDirective {
    /// `extension=` — a plain PHP module.
    Extension,
    /// `zend_extension=` — a Zend extension (Xdebug, OPcache-style).
    ZendExtension,
}

impl fmt::Display for LoadDirective {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Extension => "extension",
            Self::ZendExtension => "zend_extension",
        })
    }
}

/// One `[php] extensions` entry, resolved to the ini line that loads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ExtensionLine {
    /// The directive to emit.
    pub directive: LoadDirective,
    /// The value after `=` (a bare name, the verbatim path, or — for a
    /// relative Zend extension path — its absolute form).
    pub value: String,
    /// The library file that was inspected, when one was found.
    pub inspected: Option<PathBuf>,
}

impl ExtensionLine {
    /// The full ini line, without a trailing newline: the value is
    /// double-quoted with `\`, `"` and `$` escaped (see [`ini_quote`]).
    ///
    /// # Errors
    ///
    /// When the value contains a line break or NUL, which no ini quoting can
    /// carry (`[php] extensions` entries are validated against this, but a
    /// relative Zend extension path also embeds the working directory).
    pub fn ini_line(&self) -> anyhow::Result<String> {
        let quoted = ini_quote(&self.value).ok_or_else(|| {
            anyhow::anyhow!(
                "[php] extensions: {:?} contains a line break or NUL and cannot be written into \
                 the generated php.ini",
                self.value
            )
        })?;
        Ok(format!("{}={quoted}", self.directive))
    }
}

/// `value` as a double-quoted php.ini string that PHP reads back verbatim:
/// inside double quotes PHP unescapes `\\`, `\"` and `\$` (and only those), so
/// escaping exactly these three makes every other byte — `;`, `(`, `|`, `~`,
/// `${...}`, a Windows `C:\path` — literal. `None` for a value containing a
/// line break or NUL, which would end or truncate the line.
pub(crate) fn ini_quote(value: &str) -> Option<String> {
    if value.contains(['\n', '\r', '\0']) {
        return None;
    }
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for c in value.chars() {
        if matches!(c, '\\' | '"' | '$') {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('"');
    Some(out)
}

/// Whether the shared object at `path` is a Zend extension.
///
/// `Some(true)` when it exports — as *defined, global* symbols —
/// both `zend_extension_entry` and `extension_version_info` (ELF `.dynsym`,
/// the Mach-O symbol table where names carry a leading underscore, or a
/// PE/DLL export table), `Some(false)` when it is a parseable object without
/// them, and `None` when it cannot be read or is not a recognised object
/// format — the caller then falls back to `extension=` and lets PHP report
/// the problem. A local (non-exported) definition does not count: PHP's
/// `dlsym` cannot see it.
pub(crate) fn is_zend_extension(path: &Path) -> Option<bool> {
    use object::{Object, ObjectSymbol};

    let file = std::fs::File::open(path).ok()?;
    let mut data = Vec::new();
    file.take(MAX_INSPECT_BYTES).read_to_end(&mut data).ok()?;
    let obj = object::File::parse(&*data).ok()?;
    // A DLL's symbol table is stripped; what PHP's GetProcAddress lookup
    // sees is the export table (php_xdebug.dll exports `get_module`,
    // `zend_extension_entry` and `extension_version_info` there).
    if obj.format() == object::BinaryFormat::Pe {
        let table = obj.exports().ok()?;
        let exports = |wanted: &str| table.iter().any(|e| e.name() == wanted.as_bytes());
        return Some(exports(ZEND_EXTENSION_SYMBOL) && exports(ZEND_EXTENSION_VERSION_SYMBOL));
    }
    let exports = |wanted: &str| {
        obj.dynamic_symbols().chain(obj.symbols()).any(|sym| {
            sym.is_definition()
                && sym.is_global()
                && sym
                    .name()
                    .is_ok_and(|name| name == wanted || name.strip_prefix('_') == Some(wanted))
        })
    };
    Some(exports(ZEND_EXTENSION_SYMBOL) && exports(ZEND_EXTENSION_VERSION_SYMBOL))
}

/// Whether `entry` is a bare extension name (no directory component), which
/// PHP resolves against `extension_dir` for both directives.
fn is_bare_name(entry: &str) -> bool {
    !entry.contains('/') && !entry.contains('\\')
}

/// The files PHP would try for bare name `name` inside `dir`, in its order:
/// the name as a filename, then the platform's built name.
fn bare_name_candidates(dir: &Path, name: &str) -> Vec<PathBuf> {
    let mut out = vec![dir.join(name)];
    if cfg!(windows) {
        out.push(dir.join(format!("php_{name}.dll")));
        out.push(dir.join(format!("{name}.dll")));
    } else {
        // PHP_SHLIB_SUFFIX is "so" on Linux and macOS alike.
        out.push(dir.join(format!("{name}.so")));
    }
    out
}

/// The effective `extension_dir` for bare-name lookups: the last
/// `extension_dir` in `[php] ini_overrides` (PHP ini is last-wins), else the
/// compiled-in default.
pub(crate) fn effective_extension_dir(
    ini_overrides: &[[String; 2]],
    compiled_default: Option<&str>,
) -> Option<PathBuf> {
    ini_overrides
        .iter()
        .rev()
        .find(|[k, _]| k.trim().eq_ignore_ascii_case("extension_dir"))
        .map(|[_, v]| PathBuf::from(v.trim().trim_matches('"')))
        .or_else(|| compiled_default.filter(|d| !d.is_empty()).map(PathBuf::from))
}

/// Resolve one `[php] extensions` entry to its load line.
///
/// - **Bare name** (`"redis"`, `"xdebug"`): the name is kept (PHP searches
///   `extension_dir`); the directive follows whatever library that search
///   would find, when `extension_dir` is known and holds it.
/// - **Path**: inspected directly (relative paths against `cwd`). A Zend
///   extension is emitted with an **absolute** path; a plain module keeps the
///   entry verbatim, exactly as before.
///
/// Anything that cannot be found or inspected is emitted as `extension=` with
/// the verbatim entry — PHP then reports the real error at startup.
pub(crate) fn resolve_extension_line(
    entry: &str,
    cwd: &Path,
    extension_dir: Option<&Path>,
) -> ExtensionLine {
    let entry = entry.trim();
    if is_bare_name(entry) {
        let found = extension_dir.and_then(|dir| {
            bare_name_candidates(dir, entry).into_iter().find(|candidate| candidate.is_file())
        });
        let directive = match found.as_deref().and_then(is_zend_extension) {
            Some(true) => LoadDirective::ZendExtension,
            _ => LoadDirective::Extension,
        };
        return ExtensionLine { directive, value: entry.to_string(), inspected: found };
    }

    let as_path = Path::new(entry);
    let absolute = if as_path.is_absolute() { as_path.to_path_buf() } else { cwd.join(as_path) };
    if absolute.is_file() && is_zend_extension(&absolute) == Some(true) {
        return ExtensionLine {
            directive: LoadDirective::ZendExtension,
            value: absolute.to_string_lossy().into_owned(),
            inspected: Some(absolute),
        };
    }
    let inspected = absolute.is_file().then_some(absolute);
    ExtensionLine { directive: LoadDirective::Extension, value: entry.to_string(), inspected }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Build a minimal, valid little-endian ELF64 shared object whose
    /// `.dynsym` defines each name in `defined` (in `.data`) and references
    /// each name in `undefined` (`SHN_UNDEF`). Enough for `object` to parse —
    /// the tests never `dlopen` it.
    pub(crate) fn fake_elf_so(defined: &[&str], undefined: &[&str]) -> Vec<u8> {
        use object::write::elf::{FileHeader, SectionHeader, Sym, Writer};
        use object::{Endianness, elf};

        let mut out = Vec::new();
        let mut w = Writer::new(Endianness::Little, true, &mut out);

        w.reserve_file_header();
        // Section indexes: null(0), .data, .dynsym, .dynstr, .shstrtab.
        w.reserve_null_section_index();
        let data_name = w.add_section_name(b".data");
        let data_index = w.reserve_section_index();
        let data_off = w.reserve(16, 8);
        w.reserve_dynsym_section_index();
        w.reserve_dynstr_section_index();
        w.reserve_shstrtab_section_index();

        let names: Vec<_> = defined
            .iter()
            .chain(undefined.iter())
            .map(|n| w.add_dynamic_string(n.as_bytes()))
            .collect();
        w.reserve_null_dynamic_symbol_index();
        for _ in &names {
            w.reserve_dynamic_symbol_index();
        }
        w.reserve_dynsym();
        w.reserve_dynstr();
        w.reserve_shstrtab();
        w.reserve_section_headers();

        w.write_file_header(&FileHeader {
            os_abi: elf::ELFOSABI_NONE,
            abi_version: 0,
            e_type: elf::ET_DYN,
            e_machine: elf::EM_X86_64,
            e_entry: 0,
            e_flags: 0,
        })
        .unwrap();
        w.write_align(8);
        w.write(&[0u8; 16]);

        w.write_null_dynamic_symbol();
        for (i, name) in names.iter().enumerate() {
            let is_defined = i < defined.len();
            w.write_dynamic_symbol(&Sym {
                name: Some(*name),
                section: is_defined.then_some(data_index),
                st_info: (elf::STB_GLOBAL << 4) | elf::STT_OBJECT,
                st_other: elf::STV_DEFAULT,
                st_shndx: 0,
                st_value: 0,
                st_size: if is_defined { 8 } else { 0 },
            });
        }
        w.write_dynstr();
        w.write_shstrtab();

        w.write_null_section_header();
        w.write_section_header(&SectionHeader {
            name: Some(data_name),
            sh_type: elf::SHT_PROGBITS,
            sh_flags: u64::from(elf::SHF_ALLOC | elf::SHF_WRITE),
            sh_addr: 0,
            sh_offset: data_off as u64,
            sh_size: 16,
            sh_link: 0,
            sh_info: 0,
            sh_addralign: 8,
            sh_entsize: 0,
        });
        w.write_dynsym_section_header(0, 1);
        w.write_dynstr_section_header(0);
        w.write_shstrtab_section_header();
        out
    }

    /// Build a minimal, valid PE32+ DLL whose export table names each entry
    /// of `exports` (all pointing at one `ret` in `.text`). The DLL's COFF
    /// symbol table is empty — like a real stripped release DLL, only the
    /// export table says what it provides. Never loaded, only parsed.
    pub(crate) fn fake_pe_dll(exports: &[&str]) -> Vec<u8> {
        use object::pe;
        use object::write::pe::{NtHeaders, Writer};

        let mut names: Vec<&str> = exports.to_vec();
        names.sort_unstable(); // AddressOfNames must be sorted for lookups
        let n = u32::try_from(names.len()).unwrap();
        let dll_name = b"fake.dll\0";
        let strings_off = 40 + 4 * n + 4 * n + 2 * n;
        let strings_len: u32 =
            u32::try_from(dll_name.len() + names.iter().map(|s| s.len() + 1).sum::<usize>())
                .unwrap();

        let mut out = Vec::new();
        let mut w = Writer::new(true, 0x1000, 0x200, &mut out);
        w.reserve_dos_header_and_stub();
        w.reserve_nt_headers(16);
        w.reserve_section_headers(2);
        let text = w.reserve_text_section(16);
        let edata = w.reserve_edata_section(strings_off + strings_len);

        w.write_dos_header_and_stub().unwrap();
        w.write_nt_headers(NtHeaders {
            machine: pe::IMAGE_FILE_MACHINE_AMD64,
            time_date_stamp: 0,
            characteristics: pe::IMAGE_FILE_EXECUTABLE_IMAGE
                | pe::IMAGE_FILE_LARGE_ADDRESS_AWARE
                | pe::IMAGE_FILE_DLL,
            major_linker_version: 14,
            minor_linker_version: 0,
            address_of_entry_point: 0,
            image_base: 0x1_8000_0000,
            major_operating_system_version: 6,
            minor_operating_system_version: 0,
            major_image_version: 0,
            minor_image_version: 0,
            major_subsystem_version: 6,
            minor_subsystem_version: 0,
            subsystem: pe::IMAGE_SUBSYSTEM_WINDOWS_CUI,
            dll_characteristics: 0,
            size_of_stack_reserve: 0x10_0000,
            size_of_stack_commit: 0x1000,
            size_of_heap_reserve: 0x10_0000,
            size_of_heap_commit: 0x1000,
        });
        w.write_section_headers();
        w.write_section(text.file_offset, &[0xC3; 16]);

        // IMAGE_EXPORT_DIRECTORY, then functions, name RVAs, ordinals,
        // strings — all RVAs relative to the image base.
        let va = edata.virtual_address;
        let funcs_rva = va + 40;
        let names_rva = funcs_rva + 4 * n;
        let ords_rva = names_rva + 4 * n;
        let strings_rva = va + strings_off;
        let mut e: Vec<u8> = Vec::new();
        let u32le = |e: &mut Vec<u8>, v: u32| e.extend_from_slice(&v.to_le_bytes());
        u32le(&mut e, 0); // Characteristics
        u32le(&mut e, 0); // TimeDateStamp
        e.extend_from_slice(&[0; 4]); // Major/MinorVersion
        u32le(&mut e, strings_rva); // Name
        u32le(&mut e, 1); // Base
        u32le(&mut e, n); // NumberOfFunctions
        u32le(&mut e, n); // NumberOfNames
        u32le(&mut e, funcs_rva);
        u32le(&mut e, names_rva);
        u32le(&mut e, ords_rva);
        for _ in 0..n {
            u32le(&mut e, text.virtual_address);
        }
        let mut next = strings_rva + u32::try_from(dll_name.len()).unwrap();
        for name in &names {
            u32le(&mut e, next);
            next += u32::try_from(name.len() + 1).unwrap();
        }
        for i in 0..n {
            e.extend_from_slice(&u16::try_from(i).unwrap().to_le_bytes());
        }
        e.extend_from_slice(dll_name);
        for name in &names {
            e.extend_from_slice(name.as_bytes());
            e.push(0);
        }
        w.write_section(edata.file_offset, &e);
        out
    }

    #[test]
    fn detects_zend_extension_in_a_dll_export_table() {
        let dir = tempfile::tempdir().unwrap();
        // php_xdebug.dll's export set; a plain extension DLL exports only
        // get_module.
        let xdebug = write(
            dir.path(),
            "php_xdebug.dll",
            &fake_pe_dll(&["get_module", "zend_extension_entry", "extension_version_info"]),
        );
        let redis = write(dir.path(), "php_redis.dll", &fake_pe_dll(&["get_module"]));
        // zend_load_extension also needs extension_version_info.
        let no_version = write(
            dir.path(),
            "php_nover.dll",
            &fake_pe_dll(&["get_module", "zend_extension_entry"]),
        );
        assert_eq!(is_zend_extension(&xdebug), Some(true));
        assert_eq!(is_zend_extension(&redis), Some(false));
        assert_eq!(is_zend_extension(&no_version), Some(false));

        // Windows bare-name lookup finds php_<name>.dll in extension_dir.
        if cfg!(windows) {
            let line = resolve_extension_line("xdebug", Path::new("C:\\"), Some(dir.path()));
            assert_eq!(line.ini_line().unwrap(), "zend_extension=\"xdebug\"");
            assert_eq!(line.inspected, Some(xdebug));
        }
    }

    fn write(dir: &Path, name: &str, bytes: &[u8]) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, bytes).unwrap();
        path
    }

    #[test]
    fn detects_zend_extension_symbol() {
        let dir = tempfile::tempdir().unwrap();
        // Xdebug's real export set: both get_module and zend_extension_entry.
        let xdebug = write(
            dir.path(),
            "xdebug.so",
            &fake_elf_so(&["get_module", "zend_extension_entry", "extension_version_info"], &[]),
        );
        let redis = write(dir.path(), "redis.so", &fake_elf_so(&["get_module"], &[]));
        // A module that merely *references* the symbol is not a Zend extension.
        let refs =
            write(dir.path(), "refs.so", &fake_elf_so(&["get_module"], &["zend_extension_entry"]));
        // zend_load_extension also needs extension_version_info; without it
        // the library is not a loadable Zend extension.
        let no_version = write(
            dir.path(),
            "nover.so",
            &fake_elf_so(&["get_module", "zend_extension_entry"], &[]),
        );
        let junk = write(dir.path(), "junk.so", b"not an object file");

        assert_eq!(is_zend_extension(&xdebug), Some(true));
        assert_eq!(is_zend_extension(&redis), Some(false));
        assert_eq!(is_zend_extension(&refs), Some(false));
        assert_eq!(is_zend_extension(&no_version), Some(false));
        assert_eq!(is_zend_extension(&junk), None);
        assert_eq!(is_zend_extension(&dir.path().join("missing.so")), None);
    }

    #[test]
    fn zend_extension_path_is_emitted_absolute() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("exts")).unwrap();
        write(
            &dir.path().join("exts"),
            "xdebug.so",
            &fake_elf_so(&["zend_extension_entry", "extension_version_info"], &[]),
        );

        // Relative: PHP would resolve `zend_extension=exts/xdebug.so` against
        // extension_dir, so it must come out absolute.
        let line = resolve_extension_line("exts/xdebug.so", dir.path(), None);
        assert_eq!(line.directive, LoadDirective::ZendExtension);
        let expected = dir.path().join("exts/xdebug.so");
        assert_eq!(line.value, expected.to_string_lossy());
        assert!(Path::new(&line.value).is_absolute());
        let abs = expected.to_string_lossy().into_owned();
        assert_eq!(
            line.ini_line().unwrap(),
            format!("zend_extension={}", ini_quote(&abs).unwrap())
        );

        // Absolute input stays as given.
        let line = resolve_extension_line(&abs, Path::new("/elsewhere"), None);
        assert_eq!(line.value, abs);
        assert_eq!(line.directive, LoadDirective::ZendExtension);
    }

    #[test]
    fn plain_module_path_is_emitted_verbatim() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "redis.so", &fake_elf_so(&["get_module"], &[]));
        let line = resolve_extension_line("./redis.so", dir.path(), None);
        assert_eq!(line.ini_line().unwrap(), "extension=\"./redis.so\"");
        // A path that does not exist keeps today's behavior: PHP reports it.
        let line = resolve_extension_line("/nope/mongodb.so", dir.path(), None);
        assert_eq!(line.ini_line().unwrap(), "extension=\"/nope/mongodb.so\"");
        assert_eq!(line.inspected, None);
    }

    #[test]
    fn bare_names_follow_the_library_in_extension_dir() {
        let dir = tempfile::tempdir().unwrap();
        // Name the files the way PHP's bare-name search would find them on
        // this platform (`<name>.so`, or `php_<name>.dll` on Windows).
        let file = |name: &str| {
            if cfg!(windows) { format!("php_{name}.dll") } else { format!("{name}.so") }
        };
        write(
            dir.path(),
            &file("xdebug"),
            &fake_elf_so(&["get_module", "zend_extension_entry", "extension_version_info"], &[]),
        );
        write(dir.path(), &file("redis"), &fake_elf_so(&["get_module"], &[]));

        let cwd = Path::new("/");
        let xdebug = resolve_extension_line("xdebug", cwd, Some(dir.path()));
        assert_eq!(xdebug.ini_line().unwrap(), "zend_extension=\"xdebug\"");
        let redis = resolve_extension_line("redis", cwd, Some(dir.path()));
        assert_eq!(redis.ini_line().unwrap(), "extension=\"redis\"");
        // Unknown extension_dir: cannot inspect, keep extension=.
        assert_eq!(
            resolve_extension_line("xdebug", cwd, None).ini_line().unwrap(),
            "extension=\"xdebug\""
        );
    }

    #[test]
    fn ini_values_are_quoted_so_paths_cannot_break_the_ini() {
        // Characters PHP's ini parser treats specially outside quotes: a
        // comment, expression operators, a variable reference.
        assert_eq!(ini_quote("/a;b(c)|d&e~f!g^h").unwrap(), "\"/a;b(c)|d&e~f!g^h\"");
        // Inside quotes PHP unescapes exactly \\ \" \$ — escape those.
        assert_eq!(ini_quote(r#"/a"b\c${HOME}"#).unwrap(), r#""/a\"b\\c\${HOME}""#);
        assert_eq!(ini_quote(r"C:\ext\php_xdebug.dll").unwrap(), r#""C:\\ext\\php_xdebug.dll""#);
        for bad in ["/a\nzend_extension=/evil.so", "/a\rb", "/a\0b"] {
            assert_eq!(ini_quote(bad), None, "{bad:?}");
        }

        // A working directory with a line break would inject a directive
        // through the absolutized Zend extension path: refused, not written.
        let line = ExtensionLine {
            directive: LoadDirective::ZendExtension,
            value: "/home/x\ndisable_functions=/xdebug.so".into(),
            inspected: None,
        };
        assert!(line.ini_line().is_err());
    }

    #[test]
    fn extension_dir_prefers_last_override_then_default() {
        let overrides = vec![
            ["extension_dir".to_string(), "/a".to_string()],
            ["memory_limit".to_string(), "1G".to_string()],
            [" Extension_Dir ".to_string(), "\"/b\"".to_string()],
        ];
        assert_eq!(effective_extension_dir(&overrides, Some("/def")), Some(PathBuf::from("/b")));
        assert_eq!(effective_extension_dir(&[], Some("/def")), Some(PathBuf::from("/def")));
        assert_eq!(effective_extension_dir(&[], None), None);
        assert_eq!(effective_extension_dir(&[], Some("")), None);
    }
}

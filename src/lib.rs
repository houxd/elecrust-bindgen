pub use anyhow::anyhow;
pub use anyhow::{Context, Result};
use std::fs;
use std::path::{Path, PathBuf};

/// build script 的输出目录 `$OUT_DIR`
///
/// 生成物必须放在这里: 它按 target 和 package 隔离, 不会被另一个 target 的构建
/// 或并发的 cargo 实例 (如 rust-analyzer 的 check) 覆盖
pub fn out_dir() -> Result<PathBuf> {
    let dir = std::env::var_os("OUT_DIR")
        .context("OUT_DIR is not set, this function can only be called from a build script")?;
    Ok(PathBuf::from(dir))
}

/// 当前编译目标三元组, 由 cargo 通过 `TARGET` 传入
fn target_triple() -> Option<String> {
    std::env::var("TARGET").ok()
}

/// 是否在交叉编译
fn is_cross_compiling() -> bool {
    match (std::env::var("TARGET"), std::env::var("HOST")) {
        (Ok(target), Ok(host)) => target != host,
        _ => false,
    }
}

fn emit_rerun_if_changed(path: &str) {
    if std::env::var_os("CARGO").is_some() {
        println!("cargo:rerun-if-changed={}", path);
    }
}

#[derive(Debug, Clone)]
pub struct Binding {
    defines: Vec<String>,      // 预处理定义 "-Dxxx"
    include_dirs: Vec<String>, // 包含目录 "-Ixxx"
    headers: Vec<String>,      // 头文件列表 "xxx.h"
    sources: Vec<String>,      // 源文件列表 "xxx.c"
}

#[allow(unused)]
impl Binding {
    pub fn new() -> Self {
        Binding {
            defines: vec![],
            include_dirs: vec![],
            headers: vec![],
            sources: vec![],
        }
    }

    pub fn add_header(&mut self, header: impl AsRef<Path>) -> &mut Self {
        let path = header.as_ref().to_string_lossy().into_owned();
        emit_rerun_if_changed(&path);
        self.headers.push(path);
        self
    }

    /// 添加一个工具链自带的系统头文件, 如 `stdio.h`
    pub fn add_toolchain_header(&mut self, header: &str) -> Result<&mut Self> {
        let include_paths = Self::toolchain_include_dirs()?;
        for include_path in &include_paths {
            let full_path = Path::new(include_path.trim_start_matches("-I")).join(header);
            if full_path.exists() {
                self.headers.push(full_path.to_string_lossy().into_owned());
                return Ok(self);
            }
        }
        anyhow::bail!(
            "System header {} not found in the include paths of the C compiler for target {}:\n{}",
            header,
            target_triple().unwrap_or_else(|| "<host>".to_string()),
            include_paths.join("\n")
        );
    }

    /// 查询当前 target 所用 C 编译器的系统头文件搜索路径
    ///
    /// 编译器由 `cc` 依据 `TARGET` 解析 (例如 `thumbv7em-none-eabihf` 对应
    /// `arm-none-eabi-gcc`), 所以这里不需要写死任何三元组或工具链名字
    fn toolchain_include_dirs() -> Result<Vec<String>> {
        let compiler = cc::Build::new().try_get_compiler().map_err(|e| {
            anyhow!(
                "Failed to resolve the C compiler for target {}: {}",
                target_triple().unwrap_or_else(|| "<host>".to_string()),
                e
            )
        })?;

        // Windows 上的空输入用 nul
        let null_device = if cfg!(windows) { "nul" } else { "/dev/null" };
        let mut cmd = compiler.to_command();
        cmd.args(["-E", "-Wp,-v", "-xc", null_device]);
        let output = cmd
            .output()
            .with_context(|| format!("Failed to run {}", compiler.path().display()))?;

        let stderr = String::from_utf8_lossy(&output.stderr);
        let mut paths = Vec::new();
        let mut in_include_section = false;

        for line in stderr.lines() {
            if line.contains("#include <...> search starts here:") {
                in_include_section = true;
                continue;
            }
            if line.contains("End of search list.") {
                break;
            }
            if in_include_section {
                let path = line.trim();
                if !path.is_empty() {
                    paths.push(format!("-I{}", path));
                }
            }
        }

        Ok(paths)
    }

    pub fn add_headers(
        &mut self,
        headers: impl IntoIterator<Item = impl AsRef<Path>>,
    ) -> &mut Self {
        for header in headers {
            self.add_header(header);
        }
        self
    }

    pub fn add_source(&mut self, source: impl AsRef<Path>) -> &mut Self {
        let path = source.as_ref().to_string_lossy().into_owned();
        emit_rerun_if_changed(&path);
        self.sources.push(path);
        self
    }

    pub fn add_sources(
        &mut self,
        sources: impl IntoIterator<Item = impl AsRef<Path>>,
    ) -> &mut Self {
        for source in sources {
            self.add_source(source);
        }
        self
    }

    /// 添加预处理定义, define 不包含 -D 前缀
    pub fn add_define(&mut self, define: &str) -> Result<&mut Self> {
        self.defines.push(format!("-D{}", define));
        Ok(self)
    }

    pub fn add_include_dir(&mut self, include_dir: impl AsRef<Path>) -> Result<&mut Self> {
        self.include_dirs
            .push(format!("-I{}", include_dir.as_ref().to_string_lossy()));
        Ok(self)
    }

    /// 递归遍历 `dir_path` 下的所有子目录:
    /// - 遇到 `.c` 文件则加入 sources
    /// - 遇到 `.h` 文件则加入 headers, 并将其所在目录加入 include_dirs
    pub fn add_source_tree(&mut self, dir_path: impl AsRef<Path>) -> Result<&mut Self> {
        self.walk_source_tree(dir_path.as_ref())?;
        Ok(self)
    }

    fn walk_source_tree(&mut self, dir: &Path) -> Result<()> {
        emit_rerun_if_changed(&dir.to_string_lossy());

        // read_dir 的顺序由文件系统决定, 排序后才能保证每次生成的绑定内容一致
        let mut entries = fs::read_dir(dir)
            .with_context(|| format!("Failed to read directory {}", dir.display()))?
            .map(|entry| {
                entry
                    .map(|e| e.path())
                    .with_context(|| format!("Failed to read entry in {}", dir.display()))
            })
            .collect::<Result<Vec<_>>>()?;
        entries.sort();

        for path in entries {
            if path.is_dir() {
                self.walk_source_tree(&path)?;
            } else if path.is_file() {
                if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
                    let path_str = path.to_string_lossy().to_string();
                    if ext == "c" {
                        emit_rerun_if_changed(&path_str);
                        self.sources.push(path_str);
                    } else if ext == "h" {
                        emit_rerun_if_changed(&path_str);
                        self.headers.push(path_str);
                        if let Some(parent) = path.parent() {
                            let inc = format!("-I{}", parent.to_string_lossy());
                            if !self.include_dirs.contains(&inc) {
                                self.include_dirs.push(inc);
                            }
                        }
                    }
                }
            }
        }
        Ok(())
    }

    /// 从 sources 中去除匹配 `pattern` 的项 (支持 `*` 与 `**` 通配符)
    pub fn exclude_source(&mut self, pattern: &str) -> Result<&mut Self> {
        self.sources.retain(|s| !Self::glob_match(pattern, s));
        Ok(self)
    }

    /// 从 headers 中去除匹配 `pattern` 的项 (支持 `*` 与 `**` 通配符)
    pub fn exclude_header(&mut self, pattern: &str) -> Result<&mut Self> {
        self.headers.retain(|h| !Self::glob_match(pattern, h));
        Ok(self)
    }

    /// 从 include_dirs 中去除匹配 `pattern` 的项 (支持 `*` 与 `**` 通配符)
    /// 匹配时会去掉 `-I` 前缀
    pub fn exclude_include_dir(&mut self, pattern: &str) -> Result<&mut Self> {
        self.include_dirs
            .retain(|d| !Self::glob_match(pattern, d.trim_start_matches("-I")));
        Ok(self)
    }

    /// 通配符匹配:
    /// - `*`  匹配同一层级下的任意字符 (不跨越 `/`)
    /// - `**` 匹配任意字符 (可跨越多层 `/`)
    /// - `?`  匹配任意单个字符 (不跨越 `/`)
    fn glob_match(pattern: &str, text: &str) -> bool {
        let pattern = pattern.replace('\\', "/");
        let text = text.replace('\\', "/");
        let p: Vec<char> = pattern.chars().collect();
        let t: Vec<char> = text.chars().collect();
        Self::match_glob(&p, &t)
    }

    fn match_glob(p: &[char], t: &[char]) -> bool {
        if p.is_empty() {
            return t.is_empty();
        }
        if p[0] == '*' {
            if p.len() >= 2 && p[1] == '*' {
                // `**` 匹配任意字符, 包括 `/`
                let rest = &p[2..];
                if Self::match_glob(rest, t) {
                    return true;
                }
                for i in 0..t.len() {
                    if Self::match_glob(rest, &t[i + 1..]) {
                        return true;
                    }
                }
                return false;
            } else {
                // `*` 匹配任意字符, 但不跨越 `/`
                let rest = &p[1..];
                if Self::match_glob(rest, t) {
                    return true;
                }
                for i in 0..t.len() {
                    if t[i] == '/' {
                        break;
                    }
                    if Self::match_glob(rest, &t[i + 1..]) {
                        return true;
                    }
                }
                return false;
            }
        }
        if t.is_empty() {
            return false;
        }
        if (p[0] == '?' && t[0] != '/') || p[0] == t[0] {
            return Self::match_glob(&p[1..], &t[1..]);
        }
        false
    }

    /// 把参与构建的头文件/源文件/包含目录/宏定义清单写到 `$OUT_DIR/bindings_debug/`
    ///
    /// 这些清单本身与 target 无关, 放进 OUT_DIR 只是为了不污染源码树, 代价是每个
    /// target 各有一份。查看方式:
    ///
    /// ```text
    /// cat target/*/debug/build/<crate>-*/out/bindings_debug/sources.txt
    /// ```
    ///
    /// 设置 `ELECRUST_BINDGEN_VERBOSE=1` 后构建时会直接打印出目录的绝对路径
    pub fn output_debug_files(&self) -> Result<&Self> {
        let output_dir = out_dir()?.join("bindings_debug");

        if std::env::var_os("CARGO").is_some() {
            println!("cargo:rerun-if-env-changed=ELECRUST_BINDGEN_VERBOSE");
            if std::env::var_os("ELECRUST_BINDGEN_VERBOSE").is_some() {
                println!(
                    "cargo:warning=bindings debug files: {}",
                    output_dir.display()
                );
            }
        }

        self.output_debug_files_to(output_dir)
    }

    /// 把清单写到指定目录, 一般用 [`Binding::output_debug_files`] 即可
    pub fn output_debug_files_to(&self, output_dir: impl AsRef<Path>) -> Result<&Self> {
        let output_dir = output_dir.as_ref();
        fs::create_dir_all(output_dir).ok();

        let header_list_path = output_dir.join("headers.txt");
        fs::write(&header_list_path, self.headers.join("\n"))
            .map_err(|e| anyhow::anyhow!("Failed to write header list file: {}", e))?;
        let source_list_path = output_dir.join("sources.txt");
        fs::write(&source_list_path, self.sources.join("\n"))
            .map_err(|e| anyhow::anyhow!("Failed to write source list file: {}", e))?;
        let include_dir_list_path = output_dir.join("includes.txt");
        fs::write(&include_dir_list_path, self.include_dirs.join("\n"))
            .map_err(|e| anyhow::anyhow!("Failed to write include dir list file: {}", e))?;
        let define_list_path = output_dir.join("defines.txt");
        fs::write(&define_list_path, self.defines.join("\n"))
            .map_err(|e| anyhow::anyhow!("Failed to write define list file: {}", e))?;
        Ok(self)
    }

    /// 生成绑定到 `$OUT_DIR/<file_name>`, 在 crate 里这样引入:
    ///
    /// ```ignore
    /// #[allow(unused, non_upper_case_globals, non_camel_case_types, non_snake_case)]
    /// mod bindings {
    ///     include!(concat!(env!("OUT_DIR"), "/bindings.rs"));
    /// }
    /// ```
    pub fn generate_out(&self, file_name: &str) -> Result<&Self> {
        self.generate(out_dir()?.join(file_name))
    }

    pub fn generate(&self, output_path: impl AsRef<Path>) -> Result<&Self> {
        let output_path = output_path.as_ref();

        // 确保输出目录存在
        if let Some(parent) = output_path.parent() {
            fs::create_dir_all(parent)?;
        }

        // 使用 bindgen 生成绑定
        let mut builder = bindgen::Builder::default();

        // 添加预处理定义和包含目录
        for define in &self.defines {
            builder = builder.clang_arg(define);
        }
        for include_dir in &self.include_dirs {
            builder = builder.clang_arg(include_dir);
        }

        // 交叉编译时必须显式给出工具链自带的头文件路径, clang 找不到 newlib 之类的 libc.
        //
        // 本机编译时 bindgen 走 libclang: 若只装了 libclang.so、没有完整 clang
        // 的 resource dir, 会找不到 stddef.h / stdbool.h 等编译器内置头.
        // 把 gcc 的 builtin include 以 -idirafter 补上, 仅在 clang 自身搜不到时
        // 生效, 避免覆盖 clang 自带的头文件.
        //
        // clang 的 target 由 bindgen 从 cargo 的 TARGET 环境变量推导, 无需在此写死
        if is_cross_compiling() {
            for include_path in Self::toolchain_include_dirs()? {
                builder = builder.clang_arg(include_path);
            }
        } else {
            for include_path in Self::toolchain_include_dirs()? {
                let path = include_path.trim_start_matches("-I");
                if Path::new(path).join("stddef.h").exists() {
                    builder = builder.clang_arg("-idirafter").clang_arg(path);
                }
            }
        }

        // 添加头文件
        builder = builder.headers(self.headers.clone());

        // 配置生成选项
        let bindings = builder
            .parse_callbacks(Box::new(bindgen::CargoCallbacks::new())) // 解析回调
            .allowlist_recursively(true) // 递归允许所有类型和函数
            .generate_comments(true) // 生成注释
            // .blocklist_type("__BindgenBitfieldUnit") // 排除特定类型
            .use_core() // 使用 core 库
            .derive_default(true) // 允许默认派生
            .derive_debug(true) // 允许调试派生
            .size_t_is_usize(true) // 将 size_t 映射为 usize
            // 头文件会 #include <string.h>, bindgen 把 memcpy 等也生成出来.
            // glibc 里 size_t 常被展开成 unsigned long (c_ulong), 与 rustc 运行时
            // 符号期望的 usize 对不上, 触发 suspicious_runtime_symbol_definitions.
            // 这些函数由 C 库提供, 绑定里不需要.
            .blocklist_function("^(memcpy|memmove|memset|memcmp|strlen|bcmp|malloc|realloc|free)$")
            // glibc 的 _IO_FILE / newlib 的 __sFILE 含 bitfield, bindgen 会生成
            // transmute, 在较新 rustc 下触发 unnecessary_transmutes.
            // 本项目不直接读写 FILE 内部布局, 做成不透明类型即可.
            .opaque_type("^(_IO_FILE|__sFILE)$")
            // 这里不能用 `#![...]` 内部属性: 生成物是被 include! 到模块里的,
            // 宏展开位置不允许内部属性, lint 由使用处的 `#[allow(..)] mod bindings` 关掉
            .generate()
            .context("Unable to generate bindings")?;

        // 写入生成的绑定
        bindings
            .write_to_file(output_path)
            .with_context(|| format!("Couldn't write bindings to {}", output_path.display()))?;

        Ok(self)
    }

    pub fn compile(&self, output_lib: &str) -> Result<&Self> {
        let mut builder = cc::Build::new();

        // 添加定义
        for define in &self.defines {
            let def = define.trim_start_matches("-D");
            if let Some(pos) = def.find('=') {
                builder.define(&def[..pos], Some(&def[pos + 1..]));
            } else {
                builder.define(def, None);
            }
        }

        // 添加包含路径
        for include in &self.include_dirs {
            let path = include.trim_start_matches("-I");
            builder.include(path);
        }

        // 添加 C 文件
        for source in &self.sources {
            builder.file(source);
        }

        builder.compile(output_lib);

        Ok(self)
    }
}

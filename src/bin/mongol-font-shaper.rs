//! Binary entry point for `mongol-font-shaper`.
//! `mongol-font-shaper` 命令行可执行程序入口。

fn main() {
    std::process::exit(mongol_font_shaper::cli::run_env());
}

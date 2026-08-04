use clap::CommandFactory;
use clap_complete::{generate, Shell};
use xai_grok_pager::app::PagerArgs;

fn main() {
    let mut cmd = PagerArgs::command().name("doggy");
    let mut buf = Vec::new();
    generate(Shell::Zsh, &mut cmd, "doggy", &mut buf);
    let raw = String::from_utf8(buf).unwrap();
    println!("has prompt: {}", raw.contains("'::prompt -- "));
    println!("has line2 case: {}", raw.contains("case $line[2] in"));
    println!("has line1 case: {}", raw.contains("case $line[1] in"));
    if let Some(i) = raw.find("case $line") {
        println!("snippet: {}", &raw[i..i+200.min(raw.len()-i)]);
    }
}

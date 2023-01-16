use std::{path::PathBuf, fs::File, io::Read};

use lazy_static::lazy_static;
use regex::Regex;
use walkdir::WalkDir;

fn get_methods(file_path: &str) -> Vec<String> {
	let mut handlers: Vec<String> = vec![];

	let mut file = File::open(file_path).unwrap();
	
	let mut contents = String::new();
	file.read_to_string(&mut contents).unwrap();
	
	if contents.contains("async fn get") {
		handlers.push("get".into())
	}
	
	if contents.contains("async fn post") {
		handlers.push("post".into())
	}
	
	if contents.contains("async fn delete") {
		handlers.push("delete".into())
	}
	
	if contents.contains("async fn put") {
		handlers.push("put".into())
	}

	handlers
}

pub fn modules_path(routes_dir: &str) -> Vec<String> {
	let entries = WalkDir::new(&routes_dir)
		.into_iter()
		.filter_map(|e| e.ok())
		.filter(|it| it.file_type().is_file() && !it.file_name().to_str().unwrap().ends_with("mod.rs"))
		.map(|it| {
			let path = it.path().to_string_lossy();
			let methods = get_methods(&path);
			let relative_path = path.replace(&routes_dir, "");
			let module_path = relative_path.replace("/", "::").replace(".rs", "");

			methods.into_iter().map(|it| {
				format!("routes{module_path}::{it}")
			}).collect::<Vec<String>>()
		}).flatten().collect::<Vec<String>>();
	entries
}

pub fn actix_path(source_path_buf: PathBuf) -> String {
	let source_path = source_path_buf.to_string_lossy();
	lazy_static! {
		static ref RE: Regex = Regex::new(r"_(.*?)(/|.rs)").unwrap();
	}

	let relative_path = source_path.replace("src/routes", "");

	let step1 = RE.replace_all(&relative_path, "{$1}/").to_string();
	let step2 = step1.replace(".rs", "");
	let step3 = step2.trim_end_matches('/');

	step3.to_owned()
}
pub mod logic;

// Wasmターゲットかつテストでない場合のみ、Wasmエントリポイントを読み込む
#[cfg(all(target_arch = "wasm32", not(test)))]
mod wasm;
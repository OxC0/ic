cargo build --target wasm32-unknown-unknown --release
  #ic-cdk-optimizer target/wasm32-unknown-unknown/release/$PAC_NAME.wasm -o target/wasm32-unknown-unknown/release/$PAC_NAME-opt.wasm
ic-wasm ../../../../target/wasm32-unknown-unknown/release/ic-icrc1-ledger.wasm -o ./ic-icrc1-ledger-opt.wasm shrink --optimize O3
gzip -f -9 ./ic-icrc1-ledger-opt.wasm

#生成did文件
cargo run --bin ic-icrc1-ledger
cargo build --target wasm32-unknown-unknown --release
  #ic-cdk-optimizer target/wasm32-unknown-unknown/release/$PAC_NAME.wasm -o target/wasm32-unknown-unknown/release/$PAC_NAME-opt.wasm
#生成wasm文件
ic-wasm ../../../../target/wasm32-unknown-unknown/release/ic-icrc1-ledger.wasm   -o ./ic-icrc1-ledger-opt.wasm shrink --optimize O3
#加载did文件
ic-wasm ./ic-icrc1-ledger-opt.wasm  -o ic-icrc1-ledger-opt-did.wasm  metadata candid:service -f  ic-icrc1-ledger.did -v public
#压缩
gzip -f -9 ./ic-icrc1-ledger-opt-did.wasm

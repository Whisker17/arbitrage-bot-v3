use eyre::{bail, Result};
use reqwest::blocking::Client;
use serde_json::json;

const AGNI_ARTIFACT: &str = "contracts/out/GetAgniPoolSlot0BatchRequest.sol/GetAgniPoolSlot0BatchRequest.json";
const MOE_ARTIFACT: &str = "contracts/out/GetMoeLBPairSlot0BatchRequest.sol/GetMoeLBPairSlot0BatchRequest.json";

pub fn agni_call_and_write_csv(csv_in: &str, rpc_url: &str, block: Option<&str>, csv_out: &str) -> Result<()> {
    let payload = build_payload_with_artifact(AGNI_ARTIFACT, csv_in)?;
    let raw = eth_call(rpc_url, None, &payload, block)?;
    let decoded = decode_agni_slot0_batch(&raw)?;
    write_agni_csv(csv_out, &decoded)
}

pub fn moe_call_and_write_csv(csv_in: &str, rpc_url: &str, block: Option<&str>, csv_out: &str) -> Result<()> {
    let payload = build_payload_with_artifact(MOE_ARTIFACT, csv_in)?;
    let raw = eth_call(rpc_url, None, &payload, block)?;
    let decoded = decode_moe_slot0_batch(&raw)?;
    write_moe_csv(csv_out, &decoded)
}

fn build_payload_with_artifact(artifact_path: &str, csv_path: &str) -> Result<Vec<u8>> {
    let addresses = read_addresses_from_csv(csv_path)?;
    let mut bytecode = load_constructor_bytecode(artifact_path)?;
    let encoded = abi_encode_address_array(&addresses)?;
    bytecode.extend_from_slice(&encoded);
    Ok(bytecode)
}

fn read_addresses_from_csv(path: &str) -> Result<Vec<String>> {
    let mut rdr = csv::Reader::from_path(path)?;
    let headers = rdr.headers()?.clone();
    let idx = headers
        .iter()
        .position(|h| h.eq_ignore_ascii_case("Pair Address"))
        .ok_or_else(|| eyre::eyre!("CSV missing 'Pair Address' header"))?;
    let mut addrs = Vec::new();
    for rec in rdr.records() {
        let rec = rec?;
        let addr = rec.get(idx).unwrap_or("").trim();
        if addr.is_empty() { continue; }
        addrs.push(addr.to_string());
    }
    if addrs.is_empty() { bail!("No addresses found in CSV"); }
    Ok(addrs)
}

fn load_constructor_bytecode(artifact_path: &str) -> Result<Vec<u8>> {
    let raw = std::fs::read(artifact_path)?;
    let v: serde_json::Value = serde_json::from_slice(&raw)?;
    let obj = v["bytecode"]["object"].as_str().ok_or_else(|| eyre::eyre!("artifact missing bytecode.object"))?;
    let s = obj.trim_start_matches("0x");
    Ok(hex::decode(s)?)
}

fn abi_encode_address_array(addresses: &[String]) -> Result<Vec<u8>> {
    // ABI encoding for a single parameter of type address[]:
    // head (32 bytes offset = 0x20) || length (32) || N * (32-byte words with right-aligned 20-byte address)
    let n = addresses.len();
    let mut out = Vec::with_capacity(32 + 32 + 32 * n);
    push_u256(&mut out, 32); // offset to data
    push_u256(&mut out, n as u128); // length
    for a in addresses {
        let addr_bytes = parse_evm_address(a)?;
        let mut word = [0u8; 32];
        word[12..].copy_from_slice(&addr_bytes);
        out.extend_from_slice(&word);
    }
    Ok(out)
}

fn push_u256(buf: &mut Vec<u8>, val: u128) {
    let mut word = [0u8; 32];
    let be = val.to_be_bytes();
    word[16..].copy_from_slice(&be);
    buf.extend_from_slice(&word);
}

fn parse_evm_address(s: &str) -> Result<[u8; 20]> {
    let s = s.strip_prefix("0x").unwrap_or(s);
    let bytes = hex::decode(s)?;
    if bytes.len() != 20 { bail!("address length is not 20 bytes: {}", s.len()); }
    let mut out = [0u8; 20];
    out.copy_from_slice(&bytes);
    Ok(out)
}

fn eth_call(rpc_url: &str, to: Option<&str>, data: &[u8], block: Option<&str>) -> Result<Vec<u8>> {
    let client = Client::builder().build()?;
    let mut call = serde_json::Map::new();
    if let Some(to_addr) = to { call.insert("to".into(), serde_json::Value::String(to_addr.to_string())); }
    call.insert("data".into(), serde_json::Value::String(format!("0x{}", hex::encode(data))));
    let params = json!([serde_json::Value::Object(call), block.unwrap_or("latest")]);
    let body = json!({ "jsonrpc": "2.0", "id": 1, "method": "eth_call", "params": params });
    let resp: serde_json::Value = client.post(rpc_url).json(&body).send()?.error_for_status()?.json()?;
    let result = resp["result"].as_str().ok_or_else(|| eyre::eyre!("missing result"))?;
    Ok(hex::decode(result.trim_start_matches("0x"))?)
}

// Agni decode: returns array of Slot0Data { tick, liquidity, sqrtPrice }
#[derive(Debug, Clone)]
struct AgniSlot0Data { tick: i32, liquidity: u128, sqrt_price_hex: String }

fn decode_agni_slot0_batch(data: &[u8]) -> Result<Vec<AgniSlot0Data>> {
    // abi.decode((int24,uint128,uint256)[])
    // layout: offset(32) | length(32) | [items]
    ensure_min_len(data, 64)?;
    let offset = u256_at(data, 0)? as usize; // should be 32
    let base = offset;
    let len = u256_at(data, 32)? as usize;
    let mut out = Vec::with_capacity(len);
    let item_size = 32 * 3; // each element is a tuple, each field padded to 32
    for i in 0..len {
        let start = base + 32 + i * item_size; // array starts after length word
        let tick_word = &data[start..start + 32];
        let liquidity_word = &data[start + 32..start + 64];
        let sqrt_price_word = &data[start + 64..start + 96];
        let tick = int24_from_word(tick_word);
        let liquidity = u128_from_word(liquidity_word);
        let sqrt_price_hex = hex_from_word_trimmed(sqrt_price_word);
        out.push(AgniSlot0Data { tick, liquidity, sqrt_price_hex });
    }
    Ok(out)
}

// Moe decode: Slot0Data { activeId, binStep, reserveX, reserveY, protocolShare, maxVolatilityAccumulator }
#[derive(Debug, Clone)]
struct MoeSlot0Data { active_id: u32, bin_step: u16, reserve_x: u128, reserve_y: u128, protocol_share: u16, max_vol_acc: u32 }

fn decode_moe_slot0_batch(data: &[u8]) -> Result<Vec<MoeSlot0Data>> {
    ensure_min_len(data, 64)?;
    let offset = u256_at(data, 0)? as usize; // 32
    let base = offset;
    let len = u256_at(data, 32)? as usize;
    let mut out = Vec::with_capacity(len);
    // 6 fields, each 32 bytes padded
    let item_size = 32 * 6;
    for i in 0..len {
        let start = base + 32 + i * item_size;
        let active_id = u32_from_word(&data[start..start + 32]);
        let bin_step = u16_from_word(&data[start + 32..start + 64]);
        let reserve_x = u128_from_word(&data[start + 64..start + 96]);
        let reserve_y = u128_from_word(&data[start + 96..start + 128]);
        let protocol_share = u16_from_word(&data[start + 128..start + 160]);
        let max_vol_acc = u32_from_word(&data[start + 160..start + 192]);
        out.push(MoeSlot0Data { active_id, bin_step, reserve_x, reserve_y, protocol_share, max_vol_acc });
    }
    Ok(out)
}

fn ensure_min_len(data: &[u8], min: usize) -> Result<()> { if data.len() < min { bail!("decode: short data") } else { Ok(()) } }
fn u256_at(data: &[u8], at: usize) -> Result<u128> { Ok(u128_from_word(&data[at..at+32])) }
fn u128_from_word(word: &[u8]) -> u128 { let mut x = [0u8; 16]; x.copy_from_slice(&word[16..]); u128::from_be_bytes(x) }
fn u32_from_word(word: &[u8]) -> u32 { let mut x = [0u8; 4]; x.copy_from_slice(&word[28..]); u32::from_be_bytes(x) }
fn u16_from_word(word: &[u8]) -> u16 { let mut x = [0u8; 2]; x.copy_from_slice(&word[30..]); u16::from_be_bytes(x) }
fn int24_from_word(word: &[u8]) -> i32 {
    // grab last 3 bytes and sign-extend
    let b0 = word[29]; let b1 = word[30]; let b2 = word[31];
    let mut v = ((b0 as i32) << 16) | ((b1 as i32) << 8) | (b2 as i32);
    if (v & 0x800000) != 0 { v |= !0xFFFFFF; }
    v
}

fn hex_from_word_trimmed(word: &[u8]) -> String {
    // Trim leading zero bytes for readability; ensure at least "0"
    let mut i = 0usize;
    while i < word.len() && word[i] == 0 { i += 1; }
    let slice = if i == word.len() { &word[word.len()-1..] } else { &word[i..] };
    format!("0x{}", hex::encode(slice))
}

fn write_agni_csv(path: &str, rows: &[AgniSlot0Data]) -> Result<()> {
    let mut w = csv::Writer::from_path(path)?;
    w.write_record(["tick", "liquidity", "sqrtPriceHex"])?;
    for r in rows { w.write_record([r.tick.to_string(), r.liquidity.to_string(), r.sqrt_price_hex.clone()])?; }
    w.flush()?;
    Ok(())
}

fn write_moe_csv(path: &str, rows: &[MoeSlot0Data]) -> Result<()> {
    let mut w = csv::Writer::from_path(path)?;
    w.write_record(["activeId", "binStep", "reserveX", "reserveY", "protocolShare", "maxVolatilityAccumulator"])?;
    for r in rows { w.write_record([r.active_id.to_string(), r.bin_step.to_string(), r.reserve_x.to_string(), r.reserve_y.to_string(), r.protocol_share.to_string(), r.max_vol_acc.to_string()])?; }
    w.flush()?;
    Ok(())
}


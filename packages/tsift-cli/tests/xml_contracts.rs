//! End-to-end coverage for the XML contract dialects (`#xsdcontract`): an
//! indexed `.xsd` answers `graph --callers` through the real CLI.

use std::fs;
use std::process::Command;

fn tsift_bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_tsift-cli"))
}

fn index(root: &str) {
    let indexed = tsift_bin().args(["index", root]).output().unwrap();
    assert!(
        indexed.status.success(),
        "index stderr: {}",
        String::from_utf8_lossy(&indexed.stderr)
    );
}

fn callers(root: &str, symbol: &str) -> String {
    let graph = tsift_bin()
        .args(["graph", symbol, root, "--callers", "--json"])
        .output()
        .unwrap();
    assert!(
        graph.status.success(),
        "graph stderr: {}",
        String::from_utf8_lossy(&graph.stderr)
    );
    let json: serde_json::Value = serde_json::from_slice(&graph.stdout).unwrap();
    json.to_string()
}

const ORDERS_XSD: &str = r#"<?xml version="1.0"?>
<xsd:schema xmlns:xsd="http://www.w3.org/2001/XMLSchema"
            xmlns:tns="urn:example:orders" targetNamespace="urn:example:orders">
  <xsd:complexType name="Address">
    <xsd:sequence><xsd:element name="street" type="xsd:string"/></xsd:sequence>
  </xsd:complexType>
  <xsd:complexType name="Customer">
    <xsd:sequence><xsd:element name="billTo" type="tns:Address"/></xsd:sequence>
  </xsd:complexType>
  <xsd:complexType name="UsAddress">
    <xsd:complexContent>
      <xsd:extension base="tns:Address"/>
    </xsd:complexContent>
  </xsd:complexType>
</xsd:schema>
"#;

#[test]
fn xsd_type_callers_list_type_and_base_references() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("orders.xsd"), ORDERS_XSD).unwrap();
    // Plain XML is not walked: its look-alike schema content must not leak in.
    fs::write(
        dir.path().join("notes.xml"),
        "<note><Bogus type=\"Address\"/></note>\n",
    )
    .unwrap();
    let root = dir.path().to_str().unwrap();
    index(root);

    let address = callers(root, "Address");
    assert!(
        address.contains("Customer"),
        "a `type=` reference must be a caller edge: {address}"
    );
    assert!(
        address.contains("UsAddress"),
        "an `extension base=` reference must be a caller edge: {address}"
    );
    assert!(!address.contains("notes.xml"), "{address}");

    let search = tsift_bin()
        .args(["search", "Customer", "--path", root, "--json"])
        .output()
        .unwrap();
    assert!(search.status.success());
    let json: serde_json::Value = serde_json::from_slice(&search.stdout).unwrap();
    assert!(
        json.to_string().contains("orders.xsd"),
        "an XSD component must be a search candidate: {json}"
    );
}

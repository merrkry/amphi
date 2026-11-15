//! Reads the fields of `.drv` files that scheduling and building need.
//!
//! Store derivations use the ATerm format, which is stable across Nix
//! versions, unlike the JSON of `nix derivation show`.

use anyhow::{Context, bail, ensure};

#[derive(Debug, PartialEq)]
pub struct Derivation {
    /// Output names and paths. Paths are empty for content-addressed outputs.
    pub outputs: Vec<(String, String)>,
    /// Input derivations and the names of the outputs used.
    pub input_drvs: Vec<(String, Vec<String>)>,
    pub input_srcs: Vec<String>,
    pub system: String,
    pub required_features: Vec<String>,
}

pub fn parse(text: &str) -> anyhow::Result<Derivation> {
    let mut parser = Parser { text, pos: 0 };
    parser.expect("Derive(")?;
    let outputs = parser.list(|p| {
        p.expect("(")?;
        let name = p.string()?;
        p.expect(",")?;
        let path = p.string()?;
        for _ in 0..2 {
            p.expect(",")?;
            p.string()?;
        }
        p.expect(")")?;
        Ok((name, path))
    })?;
    parser.expect(",")?;
    let input_drvs = parser.list(|p| {
        p.expect("(")?;
        let path = p.string()?;
        p.expect(",")?;
        let outputs = p.list(Parser::string)?;
        p.expect(")")?;
        Ok((path, outputs))
    })?;
    parser.expect(",")?;
    let input_srcs = parser.list(Parser::string)?;
    parser.expect(",")?;
    let system = parser.string()?;
    parser.expect(",")?;
    parser.string()?;
    parser.expect(",")?;
    parser.list(Parser::string)?;
    parser.expect(",")?;
    let env = parser.list(|p| {
        p.expect("(")?;
        let name = p.string()?;
        p.expect(",")?;
        let value = p.string()?;
        p.expect(")")?;
        Ok((name, value))
    })?;
    parser.expect(")")?;
    ensure!(parser.pos == text.len(), "trailing data after derivation");

    Ok(Derivation {
        outputs,
        input_drvs,
        input_srcs,
        system,
        required_features: required_features(&env)?,
    })
}

fn required_features(env: &[(String, String)]) -> anyhow::Result<Vec<String>> {
    let lookup = |name: &str| {
        env.iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value)
    };
    // With structured attributes, all attributes live in one JSON variable.
    if let Some(json) = lookup("__json") {
        #[derive(serde::Deserialize)]
        struct Structured {
            #[serde(default, rename = "requiredSystemFeatures")]
            required_system_features: Vec<String>,
        }
        let structured: Structured =
            serde_json::from_str(json).context("invalid __json attribute")?;
        return Ok(structured.required_system_features);
    }
    Ok(lookup("requiredSystemFeatures")
        .map(|value| value.split_whitespace().map(str::to_owned).collect())
        .unwrap_or_default())
}

struct Parser<'a> {
    text: &'a str,
    pos: usize,
}

impl Parser<'_> {
    fn expect(&mut self, token: &str) -> anyhow::Result<()> {
        if !self.text[self.pos..].starts_with(token) {
            bail!("expected {token:?} at offset {}", self.pos);
        }
        self.pos += token.len();
        Ok(())
    }

    fn list<T>(
        &mut self,
        mut item: impl FnMut(&mut Self) -> anyhow::Result<T>,
    ) -> anyhow::Result<Vec<T>> {
        self.expect("[")?;
        let mut items = Vec::new();
        if self.text[self.pos..].starts_with(']') {
            self.pos += 1;
            return Ok(items);
        }
        loop {
            items.push(item(self)?);
            if self.text[self.pos..].starts_with(',') {
                self.pos += 1;
            } else {
                self.expect("]")?;
                return Ok(items);
            }
        }
    }

    fn string(&mut self) -> anyhow::Result<String> {
        self.expect("\"")?;
        let mut value = String::new();
        let mut chars = self.text[self.pos..].char_indices();
        while let Some((offset, c)) = chars.next() {
            match c {
                '"' => {
                    self.pos += offset + 1;
                    return Ok(value);
                }
                '\\' => {
                    let (_, escaped) = chars.next().context("unterminated escape")?;
                    value.push(match escaped {
                        'n' => '\n',
                        'r' => '\r',
                        't' => '\t',
                        other => other,
                    });
                }
                other => value.push(other),
            }
        }
        bail!("unterminated string at offset {}", self.pos)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_inputs_and_features() {
        let drv = parse(concat!(
            r#"Derive([("dev","/nix/store/d-x-dev","",""),("out","/nix/store/o-x","","")],"#,
            r#"[("/nix/store/a-a.drv",["out"]),("/nix/store/b-b.drv",["dev","out"])],"#,
            r#"["/nix/store/s-builder.sh"],"x86_64-linux","/bin/sh",["-c","echo \"hi\"\n"],"#,
            r#"[("name","x"),("requiredSystemFeatures","kvm big-parallel")])"#,
        ))
        .unwrap();
        assert_eq!(
            drv,
            Derivation {
                outputs: vec![
                    ("dev".into(), "/nix/store/d-x-dev".into()),
                    ("out".into(), "/nix/store/o-x".into()),
                ],
                input_drvs: vec![
                    ("/nix/store/a-a.drv".into(), vec!["out".into()]),
                    (
                        "/nix/store/b-b.drv".into(),
                        vec!["dev".into(), "out".into()]
                    ),
                ],
                input_srcs: vec!["/nix/store/s-builder.sh".into()],
                system: "x86_64-linux".into(),
                required_features: vec!["kvm".into(), "big-parallel".into()],
            }
        );
    }

    #[test]
    fn reads_features_from_structured_attrs() {
        let drv = parse(concat!(
            r#"Derive([("out","","r:sha256","")],[],[],"builtin","builtin:fetchurl",[],"#,
            r#"[("__json","{\"requiredSystemFeatures\":[\"nixos-test\"],\"x\":1}")])"#,
        ))
        .unwrap();
        assert_eq!(drv.system, "builtin");
        assert!(drv.input_drvs.is_empty());
        assert_eq!(drv.required_features, vec!["nixos-test".to_owned()]);
    }

    #[test]
    fn rejects_unknown_formats() {
        assert!(parse(r#"DrvWithVersion("xp-dyn-drv",[],[],[],"x","y",[],[])"#).is_err());
        assert!(parse(r#"Derive([],[],[],"x","y",[],[])junk"#).is_err());
    }
}

use std::collections::HashMap;
use std::sync::OnceLock;

pub fn palette() -> &'static HashMap<String, [u8; 3]> {
    static PALETTE: OnceLock<HashMap<String, [u8; 3]>> = OnceLock::new();
    PALETTE.get_or_init(|| {
        serde_json::from_str(include_str!("../blocks.json"))
            .expect("the bundled block catalog must be valid JSON")
    })
}

pub fn parse_block_id(value: &str) -> Result<String, String> {
    let value = value.trim();
    let id = if value.contains(':') {
        value.to_owned()
    } else {
        format!("minecraft:{value}")
    };
    if palette().contains_key(&id) {
        Ok(id)
    } else {
        Err(format!(
            "'{value}' is not in blocks.json; run oakline --list-blocks to see all supported block IDs"
        ))
    }
}

pub fn ids() -> Vec<&'static str> {
    let mut ids = palette().keys().map(String::as_str).collect::<Vec<_>>();
    ids.sort_unstable();
    ids
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn block_ids_use_the_complete_sorted_json_catalog() {
        let ids = ids();
        assert_eq!(ids.len(), palette().len());
        assert!(ids.windows(2).all(|pair| pair[0] < pair[1]));
        for id in ids {
            assert_eq!(parse_block_id(id).unwrap(), id);
        }
    }

    #[test]
    fn shorthand_is_normalized_and_unknown_blocks_are_rejected() {
        assert_eq!(parse_block_id("stone").unwrap(), "minecraft:stone");
        assert_eq!(
            parse_block_id("minecraft:stone").unwrap(),
            "minecraft:stone"
        );
        assert!(
            parse_block_id("not_a_block")
                .unwrap_err()
                .contains("--list-blocks")
        );
    }
}

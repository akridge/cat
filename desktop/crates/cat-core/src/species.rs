//! Coral species reference list and autocomplete search.
//!
//! Bundled from `cat/data/reference/list_of_coral.csv` (the same file the
//! server ingests), so species search works instantly and offline. When the
//! app is connected to a server, the server's list (with team-lead
//! enable/disable choices) replaces it.

#[derive(Clone, Debug, PartialEq)]
pub struct Species {
    pub code: String,
    pub name: String,
    pub genus: String,
    pub family: String,
    pub rank: String,
    pub active: bool,
}

const BUNDLED: &str = include_str!("../../../../cat/data/reference/list_of_coral.csv");

#[derive(Clone, Debug, Default)]
pub struct SpeciesList {
    pub items: Vec<Species>,
}

impl SpeciesList {
    pub fn bundled() -> SpeciesList {
        Self::from_csv(BUNDLED).unwrap_or_default()
    }

    pub fn from_csv(text: &str) -> anyhow::Result<SpeciesList> {
        let mut rdr = csv::Reader::from_reader(text.trim_start_matches('\u{feff}').as_bytes());
        let headers = rdr.headers()?.clone();
        let col = |name: &str| headers.iter().position(|h| h.eq_ignore_ascii_case(name));
        let (c_code, c_name) = (col("SPECIES"), col("TAXONNAME"));
        let (c_genus, c_family, c_rank, c_inactive) = (col("GENUS"), col("FAMILY"), col("RANK"), col("INACTIVE_FLAG_YN"));
        let mut items = Vec::new();
        for rec in rdr.records() {
            let rec = rec?;
            let get = |c: Option<usize>| c.and_then(|i| rec.get(i)).unwrap_or("").trim().to_string();
            let code = get(c_code).to_uppercase();
            if code.is_empty() {
                continue;
            }
            let inactive = get(c_inactive);
            items.push(Species {
                code,
                name: get(c_name),
                genus: get(c_genus),
                family: get(c_family),
                rank: get(c_rank),
                active: inactive.is_empty() || inactive == "0" || inactive.eq_ignore_ascii_case("N"),
            });
        }
        items.sort_by(|a, b| a.code.cmp(&b.code));
        Ok(SpeciesList { items })
    }

    pub fn get(&self, code: &str) -> Option<&Species> {
        let code = code.trim();
        self.items.iter().find(|s| s.code.eq_ignore_ascii_case(code))
    }

    /// Ranked search over code and scientific name. Inactive species are
    /// left out; `pinned` codes (favourites / recently used) come first
    /// among equally good matches.
    pub fn search(&self, query: &str, pinned: &[String], limit: usize) -> Vec<&Species> {
        let q = query.trim().to_lowercase();
        let mut scored: Vec<(u32, &Species)> = self
            .items
            .iter()
            .filter(|s| s.active)
            .filter_map(|s| {
                let code = s.code.to_lowercase();
                let name = s.name.to_lowercase();
                let score = if q.is_empty() {
                    if pinned.iter().any(|p| p.eq_ignore_ascii_case(&s.code)) { 0 } else { return None }
                } else if code == q {
                    0
                } else if code.starts_with(&q) {
                    1
                } else if name.starts_with(&q) {
                    2
                } else if name.split([' ', '/']).any(|w| w.starts_with(&q)) {
                    3
                } else if name.contains(&q) || code.contains(&q) {
                    4
                } else {
                    return None;
                };
                let pin = pinned.iter().position(|p| p.eq_ignore_ascii_case(&s.code));
                Some((score * 1000 + pin.map(|i| i as u32).unwrap_or(999), s))
            })
            .collect();
        scored.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.code.cmp(&b.1.code)));
        scored.into_iter().take(limit).map(|x| x.1).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundled_list_loads_and_searches() {
        let l = SpeciesList::bundled();
        assert!(l.items.len() > 300, "{}", l.items.len());
        let r = l.search("plob", &[], 5);
        assert_eq!(r[0].code, "PLOB");
        assert_eq!(r[0].name, "Porites lobata");
        // Name search: "lobata" finds PLOB by word prefix.
        assert!(l.search("lobata", &[], 10).iter().any(|s| s.code == "PLOB"));
        // Pinned codes rank first for equal matches and show on empty query.
        let pinned = vec!["POSP".to_string()];
        assert_eq!(l.search("", &pinned, 5)[0].code, "POSP");
    }
}

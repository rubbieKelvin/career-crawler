//! Offline geocoding of the messy location strings job boards use ("Lagos, Nigeria",
//! "San Francisco, CA", "Remote - EMEA"). Deterministic and network-free: a city table
//! shaped like GeoNames' cities15000 (`data/cities.tsv`) plus a country name table.

use std::sync::LazyLock;

#[derive(Debug)]
pub struct City {
    pub name: String,
    pub country: String,
    pub region: Option<String>,
    pub lat: f64,
    pub lon: f64,
    names: Vec<String>,
}

static CITIES: LazyLock<Vec<City>> = LazyLock::new(|| {
    return include_str!("../../data/cities.tsv")
        .lines()
        .filter(|l| !l.starts_with('#') && !l.trim().is_empty())
        .filter_map(|line| {
            let f: Vec<&str> = line.split('|').collect();
            let [name, country, region, lat, lon, alts] = f[..] else {
                return None;
            };
            let mut names = vec![fold(name)];
            names.extend(alts.split(',').map(fold).filter(|a| !a.is_empty()));
            return Some(City {
                name: name.to_string(),
                country: country.to_string(),
                region: (!region.is_empty()).then(|| region.to_string()),
                lat: lat.parse().ok()?,
                lon: lon.parse().ok()?,
                names,
            });
        })
        .collect();
});

/// `(code, lowercase names)`.
static COUNTRIES: LazyLock<Vec<(String, Vec<String>)>> = LazyLock::new(|| {
    return include_str!("../../data/countries.tsv")
        .lines()
        .filter(|l| !l.starts_with('#') && !l.trim().is_empty())
        .filter_map(|line| {
            let (code, names) = line.split_once('|')?;
            return Some((
                code.to_string(),
                names.split(',').map(fold).collect::<Vec<_>>(),
            ));
        })
        .collect();
});

/// Lowercase, trimmed and single-spaced, for name comparison.
fn fold(s: &str) -> String {
    return s
        .trim()
        .to_lowercase()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
}

pub fn country_by_name(name: &str) -> Option<&'static str> {
    let name = fold(name);
    return COUNTRIES
        .iter()
        .find(|(_, names)| names.contains(&name))
        .map(|(code, _)| code.as_str());
}

/// A country's common name (lowercase), by ISO code.
pub fn country_name(code: &str) -> Option<&'static str> {
    return COUNTRIES
        .iter()
        .find(|(c, _)| c == code)
        .and_then(|(_, names)| names.iter().find(|n| n.len() > 2).or(names.first()))
        .map(String::as_str);
}

pub fn is_country_code(code: &str) -> bool {
    return COUNTRIES.iter().any(|(c, _)| c == code);
}

/// The city called `name`. Several cities can share a name (Birmingham, Portland), so a
/// known `country` narrows it, and otherwise the table's earlier (more prominent) row wins.
pub fn city(name: &str, country: Option<&str>) -> Option<&'static City> {
    let name = fold(name);
    let mut matches = CITIES.iter().filter(|c| c.names.contains(&name));
    return match country {
        Some(cc) => matches.find(|c| c.country == cc),
        None => matches.next(),
    };
}

/// Great-circle distance in kilometres between two coordinates.
pub fn distance_km(a: (f64, f64), b: (f64, f64)) -> f64 {
    let (lat1, lon1, lat2, lon2) = (
        a.0.to_radians(),
        a.1.to_radians(),
        b.0.to_radians(),
        b.1.to_radians(),
    );
    let h = ((lat2 - lat1) / 2.0).sin().powi(2)
        + lat1.cos() * lat2.cos() * ((lon2 - lon1) / 2.0).sin().powi(2);
    return 2.0 * 6371.0 * h.sqrt().asin();
}

/// A `(min_lat, max_lat, min_lon, max_lon)` box that contains everything within `radius_km`
/// of `(lat, lon)`: the cheap prefilter for a radius search, since this SQLite has no trig
/// functions. The longitude span is widened by the cosine of the centre's latitude, so the box
/// is never narrower than the circle.
pub fn bbox(lat: f64, lon: f64, radius_km: f64) -> (f64, f64, f64, f64) {
    let dlat = radius_km / 111.32;
    // A minimum keeps the span finite at the poles, where a radius search means little anyway.
    let cos = lat.to_radians().cos().abs().max(0.01);
    let dlon = radius_km / (111.32 * cos);
    return (
        (lat - dlat).max(-90.0),
        (lat + dlat).min(90.0),
        (lon - dlon).max(-180.0),
        (lon + dlon).min(180.0),
    );
}

/// A place to pick in a search box. `value` is what to send back: it geocodes to exactly
/// this place ("Lagos, Nigeria").
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct Suggestion {
    pub label: String,
    pub value: String,
    /// `city` or `country`.
    pub kind: &'static str,
}

/// "united states" -> "United States".
fn title_case(s: &str) -> String {
    return s
        .split(' ')
        .map(|w| {
            let mut c = w.chars();
            return c.next().map_or(String::new(), |f| {
                f.to_uppercase().collect::<String>() + c.as_str()
            });
        })
        .collect::<Vec<_>>()
        .join(" ");
}

/// How well `query` matches `name`: 0 exact, 1 prefix, 2 a later word starts with it.
fn match_rank(name: &str, query: &str) -> Option<u8> {
    if name == query {
        return Some(0);
    }
    if name.starts_with(query) {
        return Some(1);
    }
    return name
        .split(' ')
        .skip(1)
        .any(|w| w.starts_with(query))
        .then_some(2);
}

/// Cities and countries matching what the user has typed so far, best first (ties keep the
/// city table's order, which is most prominent first).
pub fn search(query: &str, limit: usize) -> Vec<Suggestion> {
    let query = fold(query);
    if query.is_empty() {
        return Vec::new();
    }
    let mut found: Vec<(u8, u8, Suggestion)> = Vec::new();
    for city in CITIES.iter() {
        let Some(rank) = city
            .names
            .iter()
            .filter_map(|n| match_rank(n, &query))
            .min()
        else {
            continue;
        };
        let country = country_name(&city.country)
            .map(title_case)
            .unwrap_or_else(|| city.country.clone());
        found.push((
            rank,
            0,
            Suggestion {
                label: format!("{}, {country}", city.name),
                value: format!("{}, {country}", city.name),
                kind: "city",
            },
        ));
    }
    for (code, names) in COUNTRIES.iter() {
        let Some(rank) = names
            .iter()
            .filter(|n| n.len() > 2)
            .filter_map(|n| match_rank(n, &query))
            .min()
        else {
            continue;
        };
        let name = country_name(code)
            .map(title_case)
            .unwrap_or_else(|| code.clone());
        found.push((
            rank,
            1,
            Suggestion {
                label: format!("{name} (whole country)"),
                value: name,
                kind: "country",
            },
        ));
    }
    found.sort_by_key(|(rank, kind, _)| (*rank, *kind));
    return found.into_iter().take(limit).map(|(_, _, s)| s).collect();
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Place {
    pub city: Option<String>,
    pub region: Option<String>,
    pub country_code: Option<String>,
    pub lat: Option<f64>,
    pub lon: Option<f64>,
}

impl Place {
    pub fn is_empty(&self) -> bool {
        return self.city.is_none() && self.country_code.is_none();
    }

    fn of_city(city: &City) -> Self {
        return Self {
            city: Some(city.name.clone()),
            region: city.region.clone(),
            country_code: Some(city.country.clone()),
            lat: Some(city.lat),
            lon: Some(city.lon),
        };
    }
}

/// Splits a location string into its parts: commas, pipes, slashes, semicolons, brackets
/// and spaced dashes separate them.
fn segments(location: &str) -> Vec<String> {
    return location
        .replace(" - ", ",")
        .replace(" – ", ",")
        .split([',', '|', '/', ';', '(', ')', '·'])
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
}

/// Resolves a location string to the best place it names: a city (with coordinates), or
/// failing that a country. Empty when it names neither ("Remote", "Anywhere").
pub fn geocode(location: &str) -> Place {
    let segs = segments(location);
    let country = segs.iter().find_map(|s| country_by_name(s));

    for (i, seg) in segs.iter().enumerate() {
        let hint = country;
        // A following two-letter code ("San Francisco, CA") names a region, which
        // disambiguates cities that share a name.
        let state = segs
            .get(i + 1)
            .filter(|s| s.len() == 2 && s.chars().all(|c| c.is_ascii_uppercase()));
        let found = city(seg, hint).or_else(|| {
            return match (hint, state) {
                (None, Some(state)) => CITIES
                    .iter()
                    .find(|c| c.names.contains(&fold(seg)) && c.region.as_deref() == Some(state)),
                _ => None,
            };
        });
        if let Some(found) = found
            && hint.is_none_or(|cc| cc == found.country)
        {
            return Place::of_city(found);
        }
    }
    return Place {
        country_code: country.map(str::to_string),
        ..Place::default()
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn city_and_country() {
        let p = geocode("Lagos, Nigeria");
        assert_eq!(p.city.as_deref(), Some("Lagos"));
        assert_eq!(p.country_code.as_deref(), Some("NG"));
        assert!((p.lat.unwrap() - 6.52).abs() < 0.01);
    }

    #[test]
    fn bare_city_alias_and_accents() {
        assert_eq!(geocode("Bengaluru").city.as_deref(), Some("Bangalore"));
        assert_eq!(geocode("SÃO  paulo").city.as_deref(), Some("São Paulo"));
        assert_eq!(
            geocode("Sao Paulo, Brazil").country_code.as_deref(),
            Some("BR")
        );
        assert_eq!(geocode("NYC").country_code.as_deref(), Some("US"));
    }

    #[test]
    fn a_us_state_code_picks_the_us_city() {
        assert_eq!(geocode("Portland, OR").region.as_deref(), Some("OR"));
        assert_eq!(
            geocode("San Francisco, CA").country_code.as_deref(),
            Some("US")
        );
        assert_eq!(
            geocode("Birmingham, United Kingdom")
                .country_code
                .as_deref(),
            Some("GB")
        );
    }

    #[test]
    fn country_only_and_no_place() {
        let p = geocode("Nigeria");
        assert_eq!(
            (p.city, p.country_code.as_deref(), p.lat),
            (None, Some("NG"), None)
        );
        assert!(geocode("Remote").is_empty());
        assert!(geocode("Anywhere").is_empty());
        assert!(geocode("").is_empty());
    }

    #[test]
    fn a_named_country_must_agree_with_the_city() {
        // "Lagos" exists only in Nigeria here; naming Portugal must not place it there.
        let p = geocode("Lagos, Portugal");
        assert_eq!(p.city, None);
        assert_eq!(p.country_code.as_deref(), Some("PT"));
    }

    #[test]
    fn search_suggests_cities_and_countries_best_first() {
        let s = search("lag", 8);
        assert_eq!(s[0].value, "Lagos, Nigeria");
        assert_eq!(s[0].kind, "city");
        let nigeria = search("nigeria", 8);
        assert_eq!(nigeria[0].value, "Nigeria");
        assert_eq!(nigeria[0].kind, "country");
        // A suggestion's value geocodes back to the place it names.
        for q in ["lag", "new y", "sao", "bang", "united"] {
            for suggestion in search(q, 8) {
                let place = geocode(&suggestion.value);
                assert!(!place.is_empty(), "{} did not geocode", suggestion.value);
                assert_eq!(
                    place.city.is_some(),
                    suggestion.kind == "city",
                    "{}",
                    suggestion.value
                );
            }
        }
        assert_eq!(search("new york", 8)[0].value, "New York, United States");
        assert_eq!(
            search("york", 8)[0].value,
            "New York, United States",
            "a later word matches too"
        );
        assert!(search("zzzz", 8).is_empty());
        assert!(search("  ", 8).is_empty());
        assert_eq!(search("a", 3).len(), 3, "limited");
    }

    #[test]
    fn tables_parse() {
        assert!(CITIES.len() > 100);
        assert!(CITIES.iter().all(|c| is_country_code(&c.country)));
    }

    #[test]
    fn distances_and_boxes() {
        let lagos = (6.5244, 3.3792);
        let abuja = (9.0579, 7.4951);
        assert!(distance_km(lagos, lagos) < 0.001);
        let km = distance_km(lagos, abuja);
        assert!(
            (500.0..570.0).contains(&km),
            "Lagos to Abuja is ~535 km: {km}"
        );

        // The box stands for a 50 km circle: points 50 km due north and due east sit on its
        // edge, so the prefilter can't drop a job the radius check would have kept.
        let (min_lat, max_lat, min_lon, max_lon) = bbox(lagos.0, lagos.1, 50.0);
        let north = (lagos.0 + 50.0 / 111.32, lagos.1);
        let east = (
            lagos.0,
            lagos.1 + 50.0 / (111.32 * lagos.0.to_radians().cos()),
        );
        assert!(min_lat <= north.0 && max_lat >= north.0);
        assert!(min_lon <= east.1 && max_lon >= east.1);
        assert!(
            (distance_km(lagos, north) - 50.0).abs() < 0.5,
            "the box edge is 50 km out"
        );
        assert!((distance_km(lagos, east) - 50.0).abs() < 0.5);
        assert_eq!(bbox(89.0, 0.0, 500.0).1, 90.0, "the box stays on the globe");
    }
}

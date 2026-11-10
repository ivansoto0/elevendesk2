//! Languages offered for dubbing (name, code), carried over from the original ElevenDesk
//! and sorted by name so the list is easy to type-ahead through.

pub const LANGUAGES: &[(&str, &str)] = &[
	("Afrikaans", "af"),
	("Akan", "ak"),
	("Albanian", "sq"),
	("Amharic", "am"),
	("Arabic", "ar"),
	("Arabic (Egypt)", "ar-EG"),
	("Armenian", "hy"),
	("Assamese", "as"),
	("Azerbaijani", "az"),
	("Basque", "eu"),
	("Belarusian", "be"),
	("Bosnian", "bs"),
	("Bulgarian", "bg"),
	("Burmese", "my"),
	("Cantonese", "yue"),
	("Catalan", "ca"),
	("Cebuano", "ceb"),
	("Chinese", "zh"),
	("Chinese (Taiwan)", "zh-TW"),
	("Croatian", "hr"),
	("Czech", "cs"),
	("Danish", "da"),
	("Dogri", "dgo"),
	("Dutch", "nl"),
	("English", "en"),
	("English (Australia)", "en-AU"),
	("English (Canada)", "en-CA"),
	("English (United Kingdom)", "en-GB"),
	("English (United States)", "en-US"),
	("Estonian", "et"),
	("Filipino (Tagalog)", "fil"),
	("Finnish", "fi"),
	("French", "fr"),
	("French (Canada)", "fr-CA"),
	("French (France)", "fr-FR"),
	("Galician", "gl"),
	("Georgian", "ka"),
	("German", "de"),
	("Greek", "el"),
	("Gujarati", "gu"),
	("Hausa", "ha"),
	("Hebrew", "he"),
	("Hindi", "hi"),
	("Hungarian", "hu"),
	("Icelandic", "is"),
	("Indonesian", "id"),
	("Italian", "it"),
	("Japanese", "ja"),
	("Javanese", "jv"),
	("Kannada", "kn"),
	("Kazakh", "kk"),
	("Kikuyu", "ki"),
	("Kinyarwanda", "rw"),
	("Kirundi", "rn"),
	("Korean", "ko"),
	("Kyrgyz", "ky"),
	("Latvian", "lv"),
	("Lithuanian", "lt"),
	("Luganda", "lg"),
	("Macedonian", "mk"),
	("Malay", "ms"),
	("Malayalam", "ml"),
	("Mandarin Chinese", "cmn"),
	("Marathi", "mr"),
	("Mongolian", "mn"),
	("Nepali", "ne"),
	("Norwegian", "no"),
	("Persian", "fa"),
	("Polish", "pl"),
	("Portuguese", "pt"),
	("Portuguese (Brazil)", "pt-BR"),
	("Portuguese (Portugal)", "pt-PT"),
	("Punjabi", "pa"),
	("Romanian", "ro"),
	("Russian", "ru"),
	("Sepedi", "nso"),
	("Sesotho", "st"),
	("Sindhi", "sd"),
	("Slovak", "sk"),
	("Slovenian", "sl"),
	("Spanish", "es"),
	("Spanish (Argentina)", "es-AR"),
	("Spanish (Chile)", "es-CL"),
	("Spanish (Mexico)", "es-MX"),
	("Spanish (Spain)", "es-ES"),
	("Sundanese", "su"),
	("Swahili", "sw"),
	("Swati", "ss"),
	("Swedish", "sv"),
	("Tajik", "tg"),
	("Tamil", "ta"),
	("Telugu", "te"),
	("Thai", "th"),
	("Tibetan", "bo"),
	("Tsonga", "ts"),
	("Tswana", "tn"),
	("Turkish", "tr"),
	("Ukrainian", "uk"),
	("Urdu", "ur"),
	("Uyghur", "ug"),
	("Uzbek", "uz"),
	("Venda", "ve"),
	("Vietnamese", "vi"),
	("Waray", "war"),
	("Welsh", "cy"),
	("Wolof", "wo"),
	("Yoruba", "yo"),
	("Zulu", "zu"),
];

pub fn names() -> Vec<String> {
	LANGUAGES.iter().map(|(name, _)| name.to_string()).collect()
}

/// Display name for an API language code; unknown codes are shown as given.
pub fn name_for(code: &str) -> String {
	if code.is_empty() {
		return "Auto-detect".to_string();
	}
	LANGUAGES
		.iter()
		.find(|(_, known)| known.eq_ignore_ascii_case(code))
		.map_or_else(|| code.to_string(), |(name, _)| name.to_string())
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn names_codes_resolve_both_ways() {
		assert_eq!(LANGUAGES.len(), 108);
		assert_eq!(name_for("es-MX"), "Spanish (Mexico)");
		assert_eq!(name_for(""), "Auto-detect");
		assert_eq!(name_for("xx"), "xx");
	}
}

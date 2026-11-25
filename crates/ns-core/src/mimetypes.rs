//! Extension to MIME type mapping (`nsd/mimetypes.c`).

use std::collections::HashMap;

/// The default `text/html` type including AOLserver's default charset.
pub const TEXT_HTML: &str = "text/html; charset=iso-8859-1";

/// Type returned for unknown extensions unless configured otherwise.
pub const TYPE_DEFAULT: &str = "*/*";

/// Compiled-in extension table.
pub const BUILTIN_TYPES: &[(&str, &str)] = &[
    (".adp", TEXT_HTML),
    (".dci", TEXT_HTML),
    (".htm", TEXT_HTML),
    (".html", TEXT_HTML),
    (".sht", TEXT_HTML),
    (".shtml", TEXT_HTML),
    (".ai", "application/postscript"),
    (".aif", "audio/aiff"),
    (".aifc", "audio/aiff"),
    (".aiff", "audio/aiff"),
    (".ani", "application/x-navi-animation"),
    (".art", "image/x-art"),
    (".asc", "text/plain"),
    (".au", "audio/basic"),
    (".avi", "video/x-msvideo"),
    (".bcpio", "application/x-bcpio"),
    (".bin", "application/octet-stream"),
    (".bmp", "image/bmp"),
    (".cdf", "application/x-netcdf"),
    (".cgm", "image/cgm"),
    (".class", "application/octet-stream"),
    (".cpio", "application/x-cpio"),
    (".cpt", "application/mac-compactpro"),
    (".css", "text/css"),
    (".csv", "application/csv"),
    (".dcr", "application/x-director"),
    (".der", "application/x-x509-ca-cert"),
    (".dir", "application/x-director"),
    (".dll", "application/octet-stream"),
    (".dms", "application/octet-stream"),
    (".doc", "application/msword"),
    (".dp", "application/commonground"),
    (".dvi", "applications/x-dvi"),
    (".dwg", "image/vnd.dwg"),
    (".dxf", "image/vnd.dxf"),
    (".dxr", "application/x-director"),
    (".elm", "text/plain"),
    (".eml", "text/plain"),
    (".etx", "text/x-setext"),
    (".exe", "application/octet-stream"),
    (".ez", "application/andrew-inset"),
    (".fm", "application/vnd.framemaker"),
    (".gbt", "text/plain"),
    (".gif", "image/gif"),
    (".gtar", "application/x-gtar"),
    (".gz", "application/x-gzip"),
    (".hdf", "application/x-hdf"),
    (".hpgl", "application/vnd.hp-hpgl"),
    (".hqx", "application/mac-binhex40"),
    (".ice", "x-conference/x-cooltalk"),
    (".ief", "image/ief"),
    (".igs", "image/iges"),
    (".iges", "image/iges"),
    (".jfif", "image/jpeg"),
    (".jpe", "image/jpeg"),
    (".jpg", "image/jpeg"),
    (".jpeg", "image/jpeg"),
    (".js", "application/x-javascript"),
    (".kar", "audio/midi"),
    (".latex", "application/x-latex"),
    (".lha", "application/octet-stream"),
    (".ls", "application/x-javascript"),
    (".lxc", "application/vnd.ms-excel"),
    (".lzh", "application/octet-stream"),
    (".man", "application/x-troff-man"),
    (".map", "application/x-navimap"),
    (".me", "application/x-troff-me"),
    (".mesh", "model/mesh"),
    (".mid", "audio/x-midi"),
    (".midi", "audio/x-midi"),
    (".mif", "application/vnd.mif"),
    (".mocha", "application/x-javascript"),
    (".mov", "video/quicktime"),
    (".movie", "video/x-sgi-movie"),
    (".mp2", "audio/mpeg"),
    (".mp3", "audio/mpeg"),
    (".mpe", "video/mpeg"),
    (".mpeg", "video/mpeg"),
    (".mpg", "video/mpeg"),
    (".mpga", "audio/mpeg"),
    (".ms", "application/x-troff-ms"),
    (".msh", "model/mesh"),
    (".nc", "application/x-netcdf"),
    (".nvd", "application/x-navidoc"),
    (".nvm", "application/x-navimap"),
    (".oda", "application/oda"),
    (".pbm", "image/x-portable-bitmap"),
    (".pcl", "application/vnd.hp-pcl"),
    (".pclx", "application/vnd.hp-pclx"),
    (".pdb", "chemical/x-pdb"),
    (".pdf", "application/pdf"),
    (".pgm", "image/x-portable-graymap"),
    (".pgn", "application/x-chess-pgn"),
    (".pic", "image/pict"),
    (".pict", "image/pict"),
    (".pnm", "image/x-portable-anymap"),
    (".png", "image/png"),
    (".pot", "application/vnd.ms-powerpoint"),
    (".ppm", "image/x-portable-pixmap"),
    (".pps", "application/vnd.ms-powerpoint"),
    (".ppt", "application/vnd.ms-powerpoint"),
    (".ps", "application/postscript"),
    (".qt", "video/quicktime"),
    (".ra", "audio/x-realaudio"),
    (".ram", "audio/x-pn-realaudio"),
    (".ras", "image/x-cmu-raster"),
    (".rgb", "image/x-rgb"),
    (".rm", "audio/x-pn-realaudio"),
    (".roff", "application/x-troff"),
    (".rpm", "audio/x-pn-realaudio-plugin"),
    (".rtf", "application/rtf"),
    (".rtx", "text/richtext"),
    (".sda", "application/vnd.stardivision.draw"),
    (".sdc", "application/vnd.stardivision.calc"),
    (".sdd", "application/vnd.stardivision.impress"),
    (".sdp", "application/vnd.stardivision.impress"),
    (".sdw", "application/vnd.stardivision.writer"),
    (".sgl", "application/vnd.stardivision.writer-global"),
    (".sgm", "text/sgml"),
    (".sgml", "text/sgml"),
    (".sh", "application/x-sh"),
    (".shar", "application/x-shar"),
    (".silo", "model/mesh"),
    (".sit", "application/x-stuffit"),
    (".skd", "application/vnd.stardivision.math"),
    (".skm", "application/vnd.stardivision.math"),
    (".skp", "application/vnd.stardivision.math"),
    (".skt", "application/vnd.stardivision.math"),
    (".smf", "application/vnd.stardivision.math"),
    (".smi", "application/smil"),
    (".smil", "application/smil"),
    (".snd", "audio/basic"),
    (".spl", "application/x-futuresplash"),
    (".sql", "application/x-sql"),
    (".src", "application/x-wais-source"),
    (".stc", "application/vnd.sun.xml.calc.template"),
    (".std", "application/vnd.sun.xml.draw.template"),
    (".sti", "application/vnd.sun.xml.impress.template"),
    (".stl", "application/x-navistyle"),
    (".stw", "application/vnd.sun.xml.writer.template"),
    (".swf", "application/x-shockwave-flash"),
    (".sxc", "application/vnd.sun.xml.calc"),
    (".sxd", "application/vnd.sun.xml.draw"),
    (".sxg", "application/vnd.sun.xml.writer.global"),
    (".sxl", "application/vnd.sun.xml.impress"),
    (".sxm", "application/vnd.sun.xml.math"),
    (".sxw", "application/vnd.sun.xml.writer"),
    (".t", "application/x-troff"),
    (".tar", "application/x-tar"),
    (".tcl", "x-tcl"),
    (".tex", "application/x-tex"),
    (".texi", "application/x-texinfo"),
    (".texinfo", "application/x-texinfo"),
    (".text", "text/plain"),
    (".tgz", "application/x-gtar"),
    (".tif", "image/tiff"),
    (".tiff", "image/tiff"),
    (".tr", "application/x-troff"),
    (".tsv", "text/tab-separated-values"),
    (".txt", "text/plain"),
    (".ustar", "application/x-ustar"),
    (".vcd", "application/x-cdlink"),
    (".vor", "application/vnd.stardivision.writer"),
    (".vrml", "model/vrml"),
    (".wav", "audio/x-wav"),
    (".wbmp", "image/vnd.wap.wbmp"),
    (".wkb", "application/vnd.ms-excel"),
    (".wks", "application/vnd.ms-excel"),
    (".wml", "text/vnd.wap.wml"),
    (".wmlc", "application/vnd.wap.wmlc"),
    (".wmls", "text/vnd.wap.wmlscript"),
    (".wmlsc", "application/vnd.wap.wmlscript"),
    (".wrl", "model/vrml"),
    (".xbm", "image/x-xbitmap"),
    (".xls", "application/vnd.ms-excel"),
    (".xlw", "application/vnd.ms-excel"),
    (".xpm", "image/x-xpixmap"),
    (".xht", "application/xhtml+xml"),
    (".xhtml", "application/xhtml+xml"),
    (".xml", "text/xml"),
    (".xsl", "text/xml"),
    (".xyz", "chemical/x-pdb"),
    (".xwd", "image/x-xwindowdump"),
    (".z", "application/x-compress"),
    (".zip", "application/zip"),
];

/// A MIME type table with configurable defaults (`ns/mimetypes`).
#[derive(Debug, Clone)]
pub struct MimeTypes {
    types: HashMap<String, String>,
    default_type: String,
    noext_type: String,
}

impl Default for MimeTypes {
    fn default() -> Self {
        let mut types = HashMap::with_capacity(BUILTIN_TYPES.len());
        for (ext, ty) in BUILTIN_TYPES {
            types.insert((*ext).to_owned(), (*ty).to_owned());
        }
        Self {
            types,
            default_type: TYPE_DEFAULT.to_owned(),
            noext_type: TYPE_DEFAULT.to_owned(),
        }
    }
}

fn normalize_ext(ext: &str) -> String {
    let lower = ext.to_ascii_lowercase();
    if lower.starts_with('.') {
        lower
    } else {
        format!(".{lower}")
    }
}

impl MimeTypes {
    /// Built-in table.
    pub fn new() -> Self {
        Self::default()
    }

    /// Add or replace a type (`AddType`); a leading dot is optional.
    pub fn add(&mut self, ext: &str, ty: &str) {
        self.types.insert(normalize_ext(ext), ty.to_owned());
    }

    /// Apply an `ns/mimetypes` config section given as key/value pairs.
    /// The special keys `default` and `noextension` set the fallbacks.
    pub fn configure<'a, I>(&mut self, pairs: I)
    where
        I: IntoIterator<Item = (&'a str, &'a str)>,
    {
        let mut noext = None;
        for (k, v) in pairs {
            if k.eq_ignore_ascii_case("default") {
                v.clone_into(&mut self.default_type);
            } else if k.eq_ignore_ascii_case("noextension") {
                noext = Some(v.to_owned());
            } else {
                self.add(k, v);
            }
        }
        self.noext_type = noext.unwrap_or_else(|| self.default_type.clone());
    }

    /// Guess the type of `file` from its extension (`Ns_GetMimeType`).
    pub fn guess(&self, file: &str) -> &str {
        let base = file.rsplit('/').next().unwrap_or(file);
        let Some(dot) = base.rfind('.') else {
            return &self.noext_type;
        };
        self.types
            .get(&base[dot..].to_ascii_lowercase())
            .map_or(self.default_type.as_str(), String::as_str)
    }

    /// The fallback type for unknown extensions.
    pub fn default_type(&self) -> &str {
        &self.default_type
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_lookup() {
        let m = MimeTypes::new();
        assert_eq!(m.guess("/a/b/index.HTML"), TEXT_HTML);
        assert_eq!(m.guess("x.gif"), "image/gif");
        assert_eq!(m.guess("x.unknownext"), "*/*");
        assert_eq!(m.guess("/dir.d/README"), "*/*");
    }

    #[test]
    fn configured_overrides() {
        let mut m = MimeTypes::new();
        m.configure([
            ("default", "text/plain"),
            (".foo", "app/foo"),
            ("bar", "app/bar"),
        ]);
        assert_eq!(m.guess("x.foo"), "app/foo");
        assert_eq!(m.guess("x.BAR"), "app/bar");
        assert_eq!(m.guess("x.zzz"), "text/plain");
        assert_eq!(m.guess("noext"), "text/plain");
        m.configure([("noextension", "application/octet-stream")]);
        assert_eq!(m.guess("noext"), "application/octet-stream");
        assert_eq!(m.default_type(), "text/plain");
    }
}

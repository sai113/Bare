//! Which tabs are playing sound, for the 🔊 button on tabs.
//!
//! Chromium knows when a tab is audible, but CEF doesn't pass that on. So a small script in
//! every frame (embedded players included) reports when an `<audio>` or `<video>` element
//! starts or stops playing with sound. Muting uses CEF's own per-tab mute.
//!
//! Not covered: sound made only with the Web Audio API (some games). Many sites keep a
//! silent Web Audio context running, so counting those would show 🔊 on tabs that are quiet.

use crate::block::js_string;

/// Marks console messages from the audio watcher.
pub const PREFIX: &str = "\u{2063}bare-audio\u{2063}";

/// Reports `<PREFIX><frame key>\t1` when this frame starts making sound and `…\t0` when it
/// stops (or the frame goes away). The key is random per frame.
pub fn watcher_script() -> String {
    format!(
        r#"(function(){{if(window.__bareAudio)return;window.__bareAudio=1;
var P={prefix}+Math.random().toString(36).slice(2)+'\t',log=console.log.bind(console),last=false,media=new Set();
function on(){{for(var m of media)if(!m.paused&&!m.ended&&!m.muted&&m.volume>0)return true;return false}}
function check(){{var a=on();if(a!==last){{last=a;log(P+(a?'1':'0'))}}}}
function track(m){{if(m instanceof HTMLMediaElement&&!media.has(m)){{media.add(m);['playing','pause','ended','volumechange','emptied'].forEach(function(e){{m.addEventListener(e,check)}})}}}}
['play','playing','volumechange'].forEach(function(e){{document.addEventListener(e,function(ev){{track(ev.target);check()}},true)}});
var play=HTMLMediaElement.prototype.play;HTMLMediaElement.prototype.play=function(){{track(this);var r=play.apply(this,arguments);setTimeout(check,0);return r}};
addEventListener('pagehide',function(){{if(last){{last=false;log(P+'0')}}}});
}})();"#,
        prefix = js_string(PREFIX)
    )
}

/// (frame key, playing) from a watcher message; `None` if it isn't one.
pub fn parse_report(message: &str) -> Option<(&str, bool)> {
    let (key, state) = message.strip_prefix(PREFIX)?.split_once('\t')?;
    if key.is_empty() || key.len() > 20 {
        return None;
    }
    match state {
        "1" => Some((key, true)),
        "0" => Some((key, false)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reports() {
        assert_eq!(parse_report(&format!("{PREFIX}k1\t1")), Some(("k1", true)));
        assert_eq!(parse_report(&format!("{PREFIX}k1\t0")), Some(("k1", false)));
        assert_eq!(parse_report(&format!("{PREFIX}k1\tyes")), None);
        assert_eq!(parse_report(&format!("{PREFIX}\t1")), None);
        assert_eq!(parse_report("k1\t1"), None);
        assert!(watcher_script().contains("HTMLMediaElement"));
    }
}

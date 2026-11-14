//! A browser front end: the screen at 2x on a canvas, taps by clicking, a box to talk and a
//! BOOT button. A minimal HTTP/1.1 server over `std::net`, one thread, no keep-alive.

use crate::board::{command, FRAME, TEXT};
use anyhow::Result;
use std::{
    io::{BufRead, BufReader, Read, Write},
    net::{TcpListener, TcpStream},
};

const PAGE: &str = r#"<!doctype html><meta charset=utf-8><title>Ironpaw board</title>
<body style="font-family:monospace;margin:16px">
<canvas id=c width=480 height=480 style="border:1px solid #888;cursor:pointer"></canvas>
<div><input id=t size=40 placeholder="say something"> <button id=talk>Talk</button> <button id=boot>BOOT</button> <button id=pwr>PWR</button> <button id=plug>Plug</button></div>
<pre id=x></pre>
<script>
const c = document.getElementById('c'), g = c.getContext('2d'), off = document.createElement('canvas');
off.width = off.height = 240;
const send = body => fetch('/cmd', { method: 'POST', body });
async function tick() {
  try {
    const px = new Uint8ClampedArray(await (await fetch('/screen')).arrayBuffer());
    if (px.length == 240 * 240 * 4) {
      off.getContext('2d').putImageData(new ImageData(px, 240, 240), 0, 0);
      g.imageSmoothingEnabled = false;
      g.drawImage(off, 0, 0, 480, 480);
    }
    document.getElementById('x').textContent = await (await fetch('/text')).text();
  } catch (e) {}
  setTimeout(tick, 150);
}
tick();
c.onclick = e => send(`tap ${Math.floor(e.offsetX / 2)} ${Math.floor(e.offsetY / 2)}`);
const t = document.getElementById('t');
const talk = () => { if (t.value) send('talk ' + t.value); t.value = ''; };
t.onkeydown = e => e.key == 'Enter' && talk();
document.getElementById('talk').onclick = talk;
document.getElementById('boot').onclick = () => send('boot');
document.getElementById('pwr').onclick = () => send('pwr');
let plugged = false;
const plug = document.getElementById('plug');
plug.onclick = () => { plugged = !plugged; send(plugged ? 'plug' : 'unplug'); plug.textContent = plugged ? 'Unplug' : 'Plug'; };
</script>"#;

/// Starts the server on `IRONPAW_SIM_PORT` (default 7979) and prints where it is.
pub fn spawn() -> Result<()> {
    let port = std::env::var("IRONPAW_SIM_PORT").unwrap_or_else(|_| "7979".into());
    let listener = TcpListener::bind(format!("127.0.0.1:{port}"))?;
    println!("web: http://127.0.0.1:{port}");
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            if let Err(e) = serve(stream) {
                log::warn!("web: {e}");
            }
        }
    });
    Ok(())
}

fn serve(mut stream: TcpStream) -> std::io::Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut line = String::new();
    reader.read_line(&mut line)?;
    let mut parts = line.split_whitespace();
    let (method, path) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""));
    let mut length = 0;
    loop {
        let mut header = String::new();
        reader.read_line(&mut header)?;
        if header.trim().is_empty() {
            break;
        }
        if let Some((name, value)) = header.split_once(':') {
            if name.eq_ignore_ascii_case("content-length") {
                length = value.trim().parse().unwrap_or(0);
            }
        }
    }
    let mut body = vec![0; length.min(4096)];
    reader.read_exact(&mut body)?;
    let (status, kind, out): (&str, &str, Vec<u8>) = match (method, path) {
        ("GET", "/") => ("200 OK", "text/html", PAGE.into()),
        ("GET", "/screen") => ("200 OK", "application/octet-stream", FRAME.lock().unwrap().clone()),
        ("GET", "/text") => ("200 OK", "text/plain; charset=utf-8", TEXT.lock().unwrap().clone().into()),
        ("POST", "/cmd") => {
            command(String::from_utf8_lossy(&body).lines().next().unwrap_or(""));
            ("204 No Content", "text/plain", Vec::new())
        }
        _ => ("404 Not Found", "text/plain", Vec::new()),
    };
    write!(stream, "HTTP/1.1 {status}\r\nContent-Type: {kind}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", out.len())?;
    stream.write_all(&out)
}

# toy-asyncrt

非同期 Rust を理解するための小さな非同期ランタイムです。非同期ランタイムをホワイトボックスにするため、std 以外の依存は [libc crate](https://crates.io/crates/libc) のみです。

IO として TcpListener と TcpStream だけがあります。

## YouTube

このランタイムを実装し解説する動画が YouTube にあります:

<a href="https://www.youtube.com/watch?v=OWYDDBT_CjI"><img src="https://pbs.twimg.com/media/G4J1U6oXQAAi7SH?format=png&name=large"></a>

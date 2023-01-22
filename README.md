# tokio-socketcan-isotp

This library sets up to create asynchronous wrapper around [socketcan-isotp](https://github.com/marcelbuesing/socketcan-isotp)

This library is currently work in progres. Basic API is working, but your knowledne may vary. Currently not dependent on the original, but rhather on the fork of the socketcan-isotp libary. This should be resolved in the final version.

You may expirience busy waiting, when write encouters io::Error:WouldBlock.

Here is example of basic echoing server:

```rust

```

pub mod websocket;
pub fn kappa() -> u64 {
    1337*1488
}

#[cfg(test)]
mod tests {
    #[test]
    fn it_works() {
        assert_eq!(2 + 2, 4);
    }
}

pub trait LockBackend {
    fn lock(&self);
    fn try_lock(&self) -> bool;
    fn unlock(&self);
}

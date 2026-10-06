void alpha(void);
void beta(void);
void indirect(int choose) {
    void (*callback)(void) = alpha;
    if (choose) callback = beta;
    callback();
}

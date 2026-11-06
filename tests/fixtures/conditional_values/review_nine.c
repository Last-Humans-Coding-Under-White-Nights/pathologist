typedef void (*NineCCallback)(void);
void alpha(void); void beta(void); void unrelated(void);
void nine_gamma(void) {} void nine_delta(void) {}
struct NineCArray { NineCCallback f; NineCCallback other; };
void nine_positional_before_designator(int c) {
    struct NineCArray a[3]={{alpha,alpha},{beta,beta},[2]={c?nine_gamma:nine_delta,alpha}};
    a[0].f();
}

struct TenCast { int x; };
typedef void (*TenCast)(void);
void alpha(void); void beta(void);
void ten_c_tag_cast(int c) { TenCast f=(TenCast)(c ? alpha : beta); f(); }
TenCast ten_c_get(void) { return alpha; }
void ten_c_comma_cast(void) { TenCast f=(TenCast)(ten_c_get(),beta); f(); }
typedef void (*TenCb)(void);
typedef struct TenShared { TenCb f; } *TenShared;
void ten_c_shared_tag(int c) { struct TenShared a={alpha}, b={beta}; TenShared p=(TenShared)(c ? &a : &b); p->f(); }

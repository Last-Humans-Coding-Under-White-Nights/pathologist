typedef void (*FourCFunction)(void);
void alpha(void);
void beta(void);
union FourScalarUnion { FourCFunction a; FourCFunction b; };
union FourScalarUnion four_c_table[1] = {{alpha}};
void four_c_union_read(void) { four_c_table[0].b(); }
union FourWriteUnion { FourCFunction a; FourCFunction b; };
void four_c_union_write(void) {
    union FourWriteUnion t[1] = {{alpha}};
    t[0].b = beta;
    t[0].a();
}

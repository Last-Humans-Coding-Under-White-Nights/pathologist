typedef void (*OffsetReviewCF)(void);
void alpha(void);
void beta(void);
OffsetReviewCF offset_review_c_table[2] = {alpha, beta};
void offset_review_c_arithmetic(int i, long l) {
    OffsetReviewCF f = *(offset_review_c_table + ((int)l + i * 1));
    OffsetReviewCF g = *(offset_review_c_table + sizeof(int) / sizeof(int));
    f(); g();
}

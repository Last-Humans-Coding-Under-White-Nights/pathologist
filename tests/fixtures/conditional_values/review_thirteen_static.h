using ThirteenHeaderF=void(*)();
void alpha();
static ThirteenHeaderF thirteen_header_a=alpha;
static ThirteenHeaderF &thirteen_header_get() { return thirteen_header_a; }
static ThirteenHeaderF *thirteen_header_p=&thirteen_header_get();

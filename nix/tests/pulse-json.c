#include <assert.h>
#include <stdio.h>
#include <string.h>

/* Internal PulseAudio encoder ABI from the pinned pulsecore/json.h. */
typedef struct pa_json_encoder pa_json_encoder;
extern pa_json_encoder *pa_json_encoder_new(void);
extern void pa_json_encoder_begin_element_object(pa_json_encoder *encoder);
extern void pa_json_encoder_add_member_string(pa_json_encoder *encoder, const char *name, const char *value);
extern void pa_json_encoder_end_object(pa_json_encoder *encoder);
extern char *pa_json_encoder_to_string_free(pa_json_encoder *encoder);
extern void pa_xfree(void *pointer);

int main(void) {
    const char *value = "Myx café 日本 \"\\\b\f\n\r\t";
    const char *expected = "{\"value\":\"Myx café 日本 " "\\\"" "\\\\" "\\b\\f\\n\\r\\t" "\"}";
    pa_json_encoder *encoder = pa_json_encoder_new();
    assert(encoder != NULL);
    pa_json_encoder_begin_element_object(encoder);
    pa_json_encoder_add_member_string(encoder, "value", value);
    pa_json_encoder_end_object(encoder);
    char *encoded = pa_json_encoder_to_string_free(encoder);
    assert(encoded != NULL);
    assert(strcmp(encoded, expected) == 0);
    pa_xfree(encoded);
    puts("Native JSON preserves UTF-8, quotes, backslashes and supported control escapes");
    return 0;
}

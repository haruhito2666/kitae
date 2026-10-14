/* 
 quantum tensor sequence - spaghettification engine 
 author: Haruhito 
 */

#ifndef QTSQ_TYPES_H
#define QTSQ_TYPES_H

#include "qtsq_format.h"

qtsq_data_type_t qtsq_detect_type(const uint8_t *data, size_t size);
qtsq_strategy_t  qtsq_optimal_strategy(qtsq_data_type_t type);
const char*     qtsq_type_name(qtsq_data_type_t type);
const char*     qtsq_strategy_name(qtsq_strategy_t strategy);
int             qtsq_type_is_image(qtsq_data_type_t type);
int             qtsq_type_is_audio(qtsq_data_type_t type);
int             qtsq_type_is_text(qtsq_data_type_t type);

#endif
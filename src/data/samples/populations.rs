// ark: command line interface to Ark enclaves
// Copyright 2026 Dark Bio AG. All rights reserved.
//
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.

//! The cohort's 26 populations and their descriptions.

use super::Population;

/// Every population of the cohort, sorted by code.
#[rustfmt::skip]
pub(crate) const POPULATIONS: [Population; 26] = [
    Population { code: "ACB", description: "African Caribbean in Barbados" },
    Population { code: "ASW", description: "African Ancestry in Southwest US" },
    Population { code: "BEB", description: "Bengali in Bangladesh" },
    Population { code: "CDX", description: "Chinese Dai in Xishuangbanna, China" },
    Population { code: "CEU", description: "Utah residents with Northern and Western European ancestry" },
    Population { code: "CHB", description: "Han Chinese in Beijing, China" },
    Population { code: "CHS", description: "Southern Han Chinese, China" },
    Population { code: "CLM", description: "Colombian in Medellin, Colombia" },
    Population { code: "ESN", description: "Esan in Nigeria" },
    Population { code: "FIN", description: "Finnish in Finland" },
    Population { code: "GBR", description: "British in England and Scotland" },
    Population { code: "GIH", description: "Gujarati Indian in Houston, TX" },
    Population { code: "GWD", description: "Gambian in Western Division, The Gambia" },
    Population { code: "IBS", description: "Iberian populations in Spain" },
    Population { code: "ITU", description: "Indian Telugu in the UK" },
    Population { code: "JPT", description: "Japanese in Tokyo, Japan" },
    Population { code: "KHV", description: "Kinh in Ho Chi Minh City, Vietnam" },
    Population { code: "LWK", description: "Luhya in Webuye, Kenya" },
    Population { code: "MSL", description: "Mende in Sierra Leone" },
    Population { code: "MXL", description: "Mexican Ancestry in Los Angeles, California" },
    Population { code: "PEL", description: "Peruvian in Lima, Peru" },
    Population { code: "PJL", description: "Punjabi in Lahore, Pakistan" },
    Population { code: "PUR", description: "Puerto Rican in Puerto Rico" },
    Population { code: "STU", description: "Sri Lankan Tamil in the UK" },
    Population { code: "TSI", description: "Toscani in Italy" },
    Population { code: "YRI", description: "Yoruba in Ibadan, Nigeria" },
];

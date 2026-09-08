EESchema Schematic File Version 4
LIBS:FreeMDU_Optics
EELAYER 29 0
EELAYER END
$Descr A4 11693 8268
Sheet 1 1
Title "FreeMDU optical adapter - XIAO ESP32-C3 + SFH7250"
Comment1 "FreeMDU: RX=GPIO6 (D4), TX=GPIO7 (D5); configure firmware accordingly"
Comment2 "SFH7250 pinout per ams OSRAM datasheet v1.6"
Comment3 "R_RX=47k recommended by FreeMDU; R_TX=100R conservative direct GPIO drive"
Comment4 "KiCad 10 can open/import this fallback schematic"
$EndDescr
$Comp
L FreeMDU_Optics:XIAO_ESP32C3 A1
U 1 1 1
P 3600 3550
F 0 "A1" H 3600 4700 50  0000 C CNN
F 1 "XIAO_ESP32C3" H 3600 4600 50 0000 C CNN
	1    3600 3550
	1 0 0 -1
$EndComp
$Comp
L FreeMDU_Optics:SFH7250 U1
U 1 1 2
P 7000 3550
F 0 "U1" H 7000 4050 50 0000 C CNN
F 1 "SFH7250" H 7000 3950 50 0000 C CNN
F 2 "FreeMDU_Optics:SFH7250_Multi_TOPLED" H 7000 3550 50 0001 C CNN
F 3 "https://look.ams-osram.com/m/4b1e110b8b52f90d/original/SFH-7250.pdf" H 7000 3550 50 0001 C CNN
	1    7000 3550
	1 0 0 -1
$EndComp
$Comp
L Device:R R1
U 1 1 3
P 5750 3400
F 0 "R1" V 5543 3400 50 0000 C CNN
F 1 "100R" V 5634 3400 50 0000 C CNN
	1    5750 3400
	0 1 1 0
$EndComp
$Comp
L Device:R R2
U 1 1 4
P 7900 3000
F 0 "R2" H 7970 3046 50 0000 L CNN
F 1 "47k" H 7970 2955 50 0000 L CNN
	1    7900 3000
	1 0 0 -1
$EndComp
$Comp
L power:+3V3 #PWR01
U 1 1 5
P 7900 2700
F 0 "#PWR01" H 7900 2550 50 0001 C CNN
F 1 "+3V3" H 7915 2873 50 0000 C CNN
	1    7900 2700
	1 0 0 -1
$EndComp
$Comp
L power:GND #PWR02
U 1 1 6
P 6200 3900
F 0 "#PWR02" H 6200 3650 50 0001 C CNN
F 1 "GND" H 6205 3727 50 0000 C CNN
	1    6200 3900
	1 0 0 -1
$EndComp
Text Label 5000 3400 0    50   ~ 0
IR_TX_GPIO7
Text Label 8350 3400 0    50   ~ 0
IR_RX_GPIO6
Wire Wire Line
	5000 3400 5600 3400
Wire Wire Line
	5900 3400 5984 3400
Wire Wire Line
	5984 3400 5984 3423
Wire Wire Line
	5984 3423 5984 3430
Wire Wire Line
	5984 3430 5984 3430
Wire Wire Line
	5984 3430 6746 3430
Wire Wire Line
	6746 3677 6200 3677
Wire Wire Line
	6200 3677 6200 3900
Wire Wire Line
	7254 3430 7900 3430
Wire Wire Line
	7900 3150 7900 3430
Wire Wire Line
	7900 3430 8350 3430
Wire Wire Line
	7900 2700 7900 2850
Wire Wire Line
	7254 3677 7600 3677
Wire Wire Line
	7600 3677 7600 3900
Wire Wire Line
	7600 3900 6200 3900
Text Notes 4700 4600 0 60 ~ 0
XIAO note: FreeMDU defaults GPIO0/RX and GPIO1/TX are not broken out on XIAO ESP32-C3.\nThis design uses D4/GPIO6 = RX and D5/GPIO7 = TX.
$EndSCHEMATC
